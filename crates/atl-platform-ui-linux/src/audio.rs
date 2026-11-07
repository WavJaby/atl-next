// SPDX-License-Identifier: MIT
//! PCM output on PipeWire (plan A0): [`PipeWireHost`] is the connection, [`PipeWireOutput`] one stream on it.
//! The connection's thread loop runs the client side. Each stream's callbacks run on the server's data thread (`RT_PROCESS`), so `fill` is realtime.
//! Every call into a stream or the core holds the thread loop's lock. Waits (negotiation, start) run after the lock is released: the loop's callbacks need it to change the state being waited for.
//! `unsafe` here: pipewire-rs marks `ThreadLoopRc::new` unsafe without a stated contract, and `sched_getscheduler` in the first callback of each output.

use std::collections::HashMap;
use std::io::Cursor;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use atl_platform_ui_api::audio::{AudioError, AudioHost, PcmOutput, PcmSpec};
use pipewire::context::ContextRc;
use pipewire::core::CoreRc;
use pipewire::keys;
use pipewire::properties::properties;
use pipewire::registry::{Listener as RegistryListener, RegistryRc};
use pipewire::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{Object, Pod, Value};
use pipewire::spa::sys as spa_sys;
use pipewire::spa::utils::Direction;
use pipewire::stream::{Stream, StreamFlags, StreamListener, StreamRc, StreamState};
use pipewire::thread_loop::ThreadLoopRc;
use pipewire::types::ObjectType;

/// How long negotiation of a new output may take: the server answers in a few graph cycles, so this bounds a hang, not a latency.
const NEGOTIATE_LIMIT: Duration = Duration::from_secs(5);
/// How long a started stream may take to run. A stream the server cannot link to a sink (no default sink, no session manager) stays paused; this bound turns that into an error.
const START_LIMIT: Duration = Duration::from_secs(2);
/// Frames per `fill` call at most. The server's buffers are one quantum (a few hundred to a few thousand frames), so `fill` normally sees one call per buffer.
const SCRATCH_FRAMES: usize = 8192;

/// The client side of one server connection: the thread loop, its context and core.
/// Outputs hold a clone; the last drop releases the core under the lock and stops the loop.
struct Connection {
    /// Taken in `drop`, under the lock.
    core: Option<CoreRc>,
    loop_: ThreadLoopRc,
}

impl Drop for Connection {
    fn drop(&mut self) {
        {
            let _lock = self.loop_.lock();
            drop(self.core.take());
        }
        self.loop_.stop();
    }
}

/// A connection to the PipeWire server (the default sink is the server's choice; no device parameter).
pub struct PipeWireHost {
    connection: Rc<Connection>,
}

impl AudioHost for PipeWireHost {
    type Output = PipeWireOutput;

    fn connect() -> Result<Self, AudioError> {
        pipewire::init();
        // SAFETY: pipewire-rs documents no precondition for `ThreadLoopRc::new` (its safety section is TODO). The call creates one pw_thread_loop, named here with no properties, not yet running; it is owned by the connection and stopped and destroyed with it.
        let loop_ = unsafe { ThreadLoopRc::new(Some("atl-pcm"), None) }
            .map_err(|e| AudioError::Unavailable(format!("thread loop: {e}")))?;
        let context = ContextRc::new(&loop_, None)
            .map_err(|e| AudioError::Unavailable(format!("context: {e}")))?;
        loop_.start();
        let core = {
            let _lock = loop_.lock();
            context.connect_rc(None)
        };
        match core {
            Ok(core) => Ok(Self {
                connection: Rc::new(Connection {
                    core: Some(core),
                    loop_,
                }),
            }),
            Err(e) => {
                loop_.stop();
                Err(AudioError::Unavailable(format!("connect: {e}")))
            }
        }
    }

    fn open_output(
        &self,
        want: PcmSpec,
        fill: impl FnMut(&mut [f32]) + Send + 'static,
    ) -> Result<PipeWireOutput, AudioError> {
        let channels = usize::from(want.channels);
        if channels == 0 {
            return Err(AudioError::Stream("0 channels requested".to_owned()));
        }
        let shared = Arc::new(Shared::new(want));
        let attached = {
            let _lock = self.connection.loop_.lock();
            let core = self
                .connection
                .core
                .as_ref()
                .ok_or_else(|| AudioError::Stream("connection closed".to_owned()))?;
            let stream = StreamRc::new(
                core.clone(),
                "atl-pcm-output",
                properties! {
                    *keys::MEDIA_TYPE => "Audio",
                    *keys::MEDIA_CATEGORY => "Playback",
                    // one role until A1 maps AudioAttributes usages to roles (plan section 3)
                    *keys::MEDIA_ROLE => "Game",
                    *keys::NODE_NAME => "atl-pcm-output",
                },
            )
            .map_err(|e| stream_error("new", &e))?;
            let data = UserData {
                fill: Box::new(fill),
                scratch: vec![0.0; SCRATCH_FRAMES * channels],
                channels,
                checked: false,
                shared: Arc::clone(&shared),
            };
            let listener = stream
                .add_local_listener_with_user_data(data)
                .state_changed(|_, ud, _old, new| ud.shared.update(|s| s.phase = phase_of(new)))
                .param_changed(|_, ud, id, param| ud.negotiated(id, param))
                .process(|stream, ud| ud.process(stream))
                .register()
                .map_err(|e| stream_error("listener", &e))?;
            let mut info = AudioInfoRaw::new();
            info.set_format(AudioFormat::F32LE);
            info.set_rate(want.rate_hz);
            info.set_channels(u32::from(want.channels));
            info.set_position(positions(channels));
            let values = PodSerializer::serialize(
                Cursor::new(Vec::new()),
                &Value::Object(Object {
                    type_: spa_sys::SPA_TYPE_OBJECT_Format,
                    id: spa_sys::SPA_PARAM_EnumFormat,
                    properties: info.into(),
                }),
            )
            .map_err(|e| AudioError::Stream(format!("format pod: {e:?}")))?
            .0
            .into_inner();
            let pod = Pod::from_bytes(&values)
                .ok_or_else(|| AudioError::Stream("format pod: not a pod".to_owned()))?;
            stream
                .connect(
                    Direction::Output,
                    None,
                    StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
                    &mut [pod],
                )
                .map_err(|e| stream_error("connect", &e))?;
            // Links from this output's node show up as registry globals; the listener keeps them in `Status::links`.
            let registry = core
                .get_registry_rc()
                .map_err(|e| stream_error("registry", &e))?;
            let link_listener = registry
                .add_listener_local()
                .global({
                    let shared = Arc::clone(&shared);
                    move |global| {
                        if global.type_ != ObjectType::Link {
                            return;
                        }
                        let output = global
                            .props
                            .and_then(|props| props.get(*keys::LINK_OUTPUT_NODE))
                            .and_then(|node| node.parse::<u32>().ok());
                        if let Some(output) = output {
                            shared.update(|s| {
                                s.links.insert(global.id, output);
                            });
                        }
                    }
                })
                .global_remove({
                    let shared = Arc::clone(&shared);
                    move |id| {
                        shared.update(|s| {
                            s.links.remove(&id);
                        });
                    }
                })
                .register();
            Attached {
                _link_listener: link_listener,
                _registry: registry,
                _listener: listener,
                stream,
            }
        };
        let output = PipeWireOutput {
            stream: Some(attached),
            shared: Arc::clone(&shared),
            connection: Rc::clone(&self.connection),
        };
        // `output` exists before the wait: its error path drops it, which takes the lock.
        shared
            .wait_for(
                NEGOTIATE_LIMIT,
                |s| s.phase == Phase::Paused,
                |s| format!("still {:?}", s.phase),
            )
            .map_err(AudioError::Stream)?;
        // The quantum is the server's request size, which only a running cycle shows: one short start measures it, so it is known before the caller asks. The link wait in set_active makes an output with no sink fail here.
        output.set_active(true)?;
        let started = Instant::now();
        while output.shared.quantum.load(Ordering::Relaxed) == 0 {
            if started.elapsed() > START_LIMIT {
                return Err(AudioError::Stream(format!(
                    "no server cycle within {START_LIMIT:?}"
                )));
            }
            thread::sleep(Duration::from_millis(1));
        }
        output.set_active(false)?;
        Ok(output)
    }
}

/// One output stream on a [`PipeWireHost`].
pub struct PipeWireOutput {
    /// Taken in `drop`, under the lock.
    stream: Option<Attached>,
    shared: Arc<Shared>,
    connection: Rc<Connection>,
}

/// One output's server objects. Fields drop in order: the link listener, the registry, the stream listener, the stream.
/// The first three are held for that drop order only, hence the underscores.
struct Attached {
    _link_listener: RegistryListener,
    _registry: RegistryRc,
    _listener: StreamListener<UserData>,
    stream: StreamRc,
}

impl PipeWireOutput {
    fn stream(&self) -> Result<&StreamRc, AudioError> {
        self.stream
            .as_ref()
            .map(|attached| &attached.stream)
            .ok_or_else(|| AudioError::Stream("output closed".to_owned()))
    }
}

impl PcmOutput for PipeWireOutput {
    fn spec(&self) -> PcmSpec {
        self.shared.lock().spec
    }

    fn quantum(&self) -> u32 {
        self.shared.quantum.load(Ordering::Relaxed)
    }

    fn set_active(&self, on: bool) -> Result<(), AudioError> {
        let node = {
            let _lock = self.connection.loop_.lock();
            let stream = self.stream()?;
            let node = stream.node_id();
            stream
                .set_active(on)
                .map_err(|e| stream_error(if on { "start" } else { "stop" }, &e))?;
            node
        };
        if !on {
            return Ok(());
        }
        let started = self.shared.wait_for(
            START_LIMIT,
            |s| s.phase == Phase::Streaming && s.linked(node),
            |s| match &s.phase {
                Phase::Streaming => format!(
                    "node {node} streams but no link reaches a sink (no default sink, or no session manager)"
                ),
                phase => format!("still {phase:?}"),
            },
        );
        if let Err(reason) = started {
            // A stream with no sink must not keep running into nothing.
            let _lock = self.connection.loop_.lock();
            if let Ok(stream) = self.stream() {
                // Best effort: the start already failed, and that error is the one reported.
                let _ = stream.set_active(false);
            }
            return Err(AudioError::Stream(reason));
        }
        Ok(())
    }

    fn rt_fallbacks(&self) -> u64 {
        self.shared.rt_fallbacks.load(Ordering::Relaxed)
    }

    fn underruns(&self) -> u64 {
        self.shared.underruns.load(Ordering::Relaxed)
    }
}

impl Drop for PipeWireOutput {
    fn drop(&mut self) {
        let _lock = self.connection.loop_.lock();
        drop(self.stream.take());
    }
}

/// The caller's sample generator, moved onto the server's realtime thread.
type Fill = Box<dyn FnMut(&mut [f32]) + Send>;

/// What the stream's callbacks reach: the fill, its scratch buffer (allocated when the output opens, never in a callback) and the state the caller reads.
struct UserData {
    fill: Fill,
    scratch: Vec<f32>,
    channels: usize,
    /// The realtime check ran (first callback).
    checked: bool,
    shared: Arc<Shared>,
}

impl UserData {
    /// The server's format reply: the granted spec, or a failure when the server granted something other than F32.
    fn negotiated(&mut self, id: u32, param: Option<&Pod>) {
        if id != spa_sys::SPA_PARAM_Format {
            return;
        }
        let Some(param) = param else {
            return;
        };
        let mut info = AudioInfoRaw::new();
        if info.parse(param).is_err() {
            return;
        }
        let Ok(channels) = u16::try_from(info.channels()) else {
            return;
        };
        let format = info.format();
        let rate_hz = info.rate();
        self.shared.update(|s| {
            if format == AudioFormat::F32LE {
                s.spec = PcmSpec { rate_hz, channels };
            } else {
                s.phase = Phase::Failed(format!("server granted {format:?}, not F32LE"));
            }
        });
    }

    /// Fills the server's buffer with `fill`'s samples, in scratch-sized calls. Runs on the server's realtime thread.
    /// The frames asked for (`requested`, the graph's quantum) are filled, not the whole buffer: a buffer fills up to its allocated size and would add that much latency.
    fn process(&mut self, stream: &Stream) {
        let Some(mut buffer) = stream.dequeue_buffer() else {
            // the server asked for a cycle and no buffer was queued: the sound is cut here
            self.shared.underruns.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let requested = buffer.requested() as usize;
        let Some(data) = buffer.datas_mut().first_mut() else {
            return;
        };
        if !self.checked {
            self.checked = true;
            if !realtime() {
                self.shared.rt_fallbacks.store(1, Ordering::Relaxed);
            }
        }
        let frame_bytes = self.channels * std::mem::size_of::<f32>();
        let Some(bytes) = data.data() else {
            *data.chunk_mut().size_mut() = 0;
            return;
        };
        let capacity = bytes.len() / frame_bytes;
        let frames = if requested == 0 {
            capacity
        } else {
            requested.min(capacity)
        };
        if frames > 0 {
            self.shared
                .quantum
                .store(frames as u32, Ordering::Relaxed);
        }
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(SCRATCH_FRAMES);
            let scratch = &mut self.scratch[..n * self.channels];
            (self.fill)(scratch);
            let out = &mut bytes[done * frame_bytes..(done + n) * frame_bytes];
            let (quads, _) = out.as_chunks_mut::<4>();
            for (dst, sample) in quads.iter_mut().zip(scratch.iter()) {
                *dst = sample.to_ne_bytes();
            }
            done += n;
        }
        let chunk = data.chunk_mut();
        *chunk.offset_mut() = 0;
        *chunk.stride_mut() = frame_bytes as i32;
        *chunk.size_mut() = (frames * frame_bytes) as u32;
    }
}

/// State the server's callbacks write and the caller waits on.
struct Shared {
    status: Mutex<Status>,
    changed: Condvar,
    rt_fallbacks: AtomicU64,
    underruns: AtomicU64,
    /// Frames in the latest server cycle (0 until one ran).
    quantum: AtomicU32,
}

struct Status {
    phase: Phase,
    spec: PcmSpec,
    /// Link object id -> the node its audio comes from, for every link the registry lists.
    links: HashMap<u32, u32>,
}

impl Status {
    fn linked(&self, node: u32) -> bool {
        self.links.values().any(|output| *output == node)
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Phase {
    Connecting,
    Paused,
    Streaming,
    Failed(String),
}

impl Shared {
    fn new(want: PcmSpec) -> Self {
        Self {
            status: Mutex::new(Status {
                phase: Phase::Connecting,
                spec: want,
                links: HashMap::new(),
            }),
            changed: Condvar::new(),
            rt_fallbacks: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            quantum: AtomicU32::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn update(&self, change: impl FnOnce(&mut Status)) {
        change(&mut self.lock());
        self.changed.notify_all();
    }

    /// Waits until `done` holds, the stream fails, or `limit` passes. A timeout reports `reason` of the status the wait gave up in.
    fn wait_for(
        &self,
        limit: Duration,
        done: impl Fn(&Status) -> bool,
        reason: impl FnOnce(&Status) -> String,
    ) -> Result<(), String> {
        let (status, _) = self
            .changed
            .wait_timeout_while(self.lock(), limit, |s| {
                !done(&*s) && !matches!(s.phase, Phase::Failed(_))
            })
            .unwrap_or_else(PoisonError::into_inner);
        match &status.phase {
            Phase::Failed(failure) => Err(failure.clone()),
            _ if done(&status) => Ok(()),
            _ => Err(format!("{} after {limit:?}", reason(&status))),
        }
    }
}

fn phase_of(state: StreamState) -> Phase {
    match state {
        StreamState::Error(reason) => Phase::Failed(reason),
        StreamState::Streaming => Phase::Streaming,
        StreamState::Paused => Phase::Paused,
        StreamState::Unconnected | StreamState::Connecting => Phase::Connecting,
    }
}

fn stream_error(what: &str, e: &pipewire::Error) -> AudioError {
    AudioError::Stream(format!("{what}: {e}"))
}

/// Channel positions for the layouts the gates use (mono, stereo); other counts are left to the server.
fn positions(channels: usize) -> [u32; spa_sys::SPA_AUDIO_MAX_CHANNELS as usize] {
    let mut position = [0; spa_sys::SPA_AUDIO_MAX_CHANNELS as usize];
    match channels {
        1 => position[0] = spa_sys::SPA_AUDIO_CHANNEL_MONO,
        2 => {
            position[0] = spa_sys::SPA_AUDIO_CHANNEL_FL;
            position[1] = spa_sys::SPA_AUDIO_CHANNEL_FR;
        }
        _ => {}
    }
    position
}

/// Whether the calling thread has a realtime policy (SCHED_FIFO or SCHED_RR). `sched_getscheduler` fails with -1, which is not realtime.
fn realtime() -> bool {
    // SAFETY: pid 0 names the calling thread; the call takes no pointers and only reads the policy.
    let policy = unsafe { libc::sched_getscheduler(0) };
    policy == libc::SCHED_FIFO || policy == libc::SCHED_RR
}
