// SPDX-License-Identifier: MIT
//! Does the compositor import a client dmabuf: one gbm buffer from a render node, sent through `zwp_linux_buffer_params_v1.create` (linux-dmabuf-unstable-v1.xml), whose refusal is the `failed` event,
//! not the protocol error `create_immed` raises (a GTK toplevel's first frame through Mesa's EGL, which ends the client: Gdk Error 71). A remote run asks this before atld starts (remote.rs `dmabuf_probe_sh`).
//! Capability, not identity: wlroots 0.17.1 labwc on PowerVR AXM advertises AR24 / XR24 and refuses every buffer (measured 2026-11-28, `Failed to import DMA-BUF FD: Invalid argument`); wlroots 0.19 on BXM imports.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use atl_wayland_wire::{Arg, Client, op as wire_op, string, word, word_after_string};

/// The interface probed; version 3 has `create` with its `created` / `failed` events and everything used here.
const DMABUF: &str = "zwp_linux_dmabuf_v1";
const DMABUF_VERSION: u32 = 3;
/// drm_fourcc.h `DRM_FORMAT_ARGB8888` ('AR24'): what GTK's toplevel sends (create_immed format 875713089 in the sifive log).
pub const ARGB8888: u32 = 0x3432_5241;
/// The probe buffer's size: small, any size the compositor takes.
const EDGE: u32 = 64;

/// Opcodes from linux-dmabuf-unstable-v1.xml (requests and events numbered separately, declaration order) and wayland.xml's wl_buffer.
mod op {
    pub const DMABUF_CREATE_PARAMS: u16 = 1;
    pub const PARAMS_ADD: u16 = 1;
    pub const PARAMS_CREATE: u16 = 2;
    pub const PARAMS_CREATED: u16 = 0;
    pub const PARAMS_FAILED: u16 = 1;
    pub const BUFFER_DESTROY: u16 = 0;
}

/// One single-plane dmabuf as `zwp_linux_buffer_params_v1.add` sends it.
pub struct Buffer {
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub offset: u32,
    pub stride: u32,
    pub modifier: u64,
}

impl Buffer {
    fn describe(&self) -> String {
        format!(
            "{}x{} fourcc {:#010x} stride {} modifier {:#x}",
            self.width, self.height, self.format, self.stride, self.modifier
        )
    }
}

/// The probe's answer, as its stdout line ([`Answer::line`]).
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    Imported(String),
    /// The compositor answered `failed`: it imports no client dmabuf, and a GTK GL toplevel ends at its first frame.
    Refused(String),
    /// Nothing to ask: no linux-dmabuf global, no render node, no gbm buffer. A client then has no dmabuf to send (Mesa's EGL falls back to its own path).
    NotProbed(String),
}

/// The prefix of a refusal's line: the run script greps for it.
pub const REFUSED_PREFIX: &str = "dmabuf: refused ";

impl Answer {
    pub fn line(&self) -> String {
        match self {
            Answer::Imported(b) => format!("dmabuf: imported {b}"),
            Answer::Refused(b) => format!("{REFUSED_PREFIX}{b}"),
            Answer::NotProbed(why) => format!("dmabuf: not probed: {why}"),
        }
    }
}

/// Allocates the probe buffer on the first render node under `dri` that gbm opens, then asks the compositor on `stream`.
pub fn probe(stream: UnixStream, dri: &Path) -> Result<Answer> {
    let buffer = match allocate(dri) {
        Ok(b) => b,
        Err(e) => return Ok(Answer::NotProbed(format!("{e:#}"))),
    };
    import(stream, buffer)
}

/// The render nodes under `dri` (`renderD*`), sorted.
fn render_nodes(dri: &Path) -> Vec<PathBuf> {
    let mut nodes: Vec<PathBuf> = std::fs::read_dir(dri)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("renderD"))
        })
        .collect();
    nodes.sort();
    nodes
}

/// An ARGB8888 buffer for rendering, as Mesa's EGL allocates a window's (implicit modifier unless gbm names one).
fn allocate(dri: &Path) -> Result<Buffer> {
    let nodes = render_nodes(dri);
    let mut errors = Vec::new();
    for node in &nodes {
        let attempt = || -> Result<Buffer> {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(node)?;
            let dev = gbm::Device::new(file).context("gbm device")?;
            let bo = dev
                .create_buffer_object::<()>(
                    EDGE,
                    EDGE,
                    gbm::Format::Argb8888,
                    gbm::BufferObjectFlags::RENDERING,
                )
                .context("gbm_bo_create")?;
            Ok(Buffer {
                fd: bo.fd().context("gbm_bo_get_fd")?,
                width: EDGE,
                height: EDGE,
                format: ARGB8888,
                offset: bo.offset(0),
                stride: bo.stride(),
                modifier: u64::from(bo.modifier()),
            })
        };
        match attempt() {
            Ok(b) => return Ok(b),
            Err(e) => errors.push(format!("{}: {e:#}", node.display())),
        }
    }
    anyhow::ensure!(!nodes.is_empty(), "no render node in {}", dri.display());
    anyhow::bail!("no gbm buffer: {}", errors.join("; "))
}

/// Sends `buffer` through `create` and reads the compositor's answer.
pub fn import(stream: UnixStream, buffer: Buffer) -> Result<Answer> {
    let mut c = Client::new(stream)?;
    let registry = c.new_id();
    c.send(1, wire_op::DISPLAY_GET_REGISTRY, &[Arg::U(registry)]);
    let mut globals: HashMap<String, (u32, u32)> = HashMap::new();
    for (object, opcode, body) in c.roundtrip()? {
        if object == registry && opcode == wire_op::REGISTRY_GLOBAL {
            globals.insert(
                string(&body, 4)?,
                (word(&body, 0)?, word_after_string(&body, 4)?),
            );
        }
    }
    let Some(&(name, version)) = globals.get(DMABUF) else {
        return Ok(Answer::NotProbed(format!("the compositor has no {DMABUF}")));
    };
    if version < DMABUF_VERSION {
        return Ok(Answer::NotProbed(format!(
            "{DMABUF} v{version}, want v{DMABUF_VERSION}"
        )));
    }
    let dmabuf = c.new_id();
    c.send(
        registry,
        wire_op::REGISTRY_BIND,
        &[
            Arg::U(name),
            Arg::S(DMABUF),
            Arg::U(DMABUF_VERSION),
            Arg::U(dmabuf),
        ],
    );
    let params = c.new_id();
    c.send(dmabuf, op::DMABUF_CREATE_PARAMS, &[Arg::U(params)]);
    let described = buffer.describe();
    c.attach(buffer.fd);
    c.send(
        params,
        op::PARAMS_ADD,
        &[
            Arg::U(0),
            Arg::U(buffer.offset),
            Arg::U(buffer.stride),
            Arg::U((buffer.modifier >> 32) as u32),
            Arg::U(buffer.modifier as u32),
        ],
    );
    c.send(
        params,
        op::PARAMS_CREATE,
        &[
            Arg::I(buffer.width as i32),
            Arg::I(buffer.height as i32),
            Arg::U(buffer.format),
            Arg::U(0),
        ],
    );
    let events = c.roundtrip()?;
    let answer = events
        .iter()
        .filter(|(o, ..)| *o == params)
        .find_map(|(_, opcode, body)| match *opcode {
            op::PARAMS_CREATED => Some(word(body, 0).map(Some)),
            op::PARAMS_FAILED => Some(Ok(None)),
            _ => None,
        })
        .context("the compositor did not answer zwp_linux_buffer_params_v1.create")??;
    Ok(match answer {
        Some(created) => {
            c.send(created, op::BUFFER_DESTROY, &[]);
            c.roundtrip()?;
            Answer::Imported(described)
        }
        None => Answer::Refused(described),
    })
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::fd::AsFd;

    use super::*;

    /// A server-made wl_buffer id (wayland.xml: 0xff000000 up).
    const CREATED: u32 = 0xff00_0001;

    /// A compositor whose globals are `globals` (name, version) and that answers `create` with `created` (`imports`) or `failed`. Returns the requests it saw (object, opcode), and the add's fd count.
    fn compositor(
        mut s: UnixStream,
        globals: Vec<(&'static str, u32)>,
        imports: bool,
    ) -> std::thread::JoinHandle<(Vec<(u32, u16)>, usize)> {
        std::thread::spawn(move || {
            let (mut seen, mut fds) = (Vec::new(), 0);
            let mut params = 0;
            loop {
                let mut head = [0u8; 8];
                // fds ride on the first bytes of a sendmsg: count them as they come
                let mut space = [std::mem::MaybeUninit::uninit(); 64];
                let mut control = rustix::net::RecvAncillaryBuffer::new(&mut space);
                let got = rustix::net::recvmsg(
                    s.as_fd(),
                    &mut [std::io::IoSliceMut::new(&mut head)],
                    &mut control,
                    rustix::net::RecvFlags::WAITALL,
                );
                for m in control.drain() {
                    if let rustix::net::RecvAncillaryMessage::ScmRights(r) = m {
                        fds += r.count();
                    }
                }
                match got {
                    Ok(r) if r.bytes == 8 => {}
                    _ => return (seen, fds),
                }
                let object = word(&head, 0).expect("object");
                let second = word(&head, 4).expect("size");
                let mut body = vec![0u8; (second >> 16) as usize - 8];
                s.read_exact(&mut body).expect("body");
                let opcode = (second & 0xffff) as u16;
                seen.push((object, opcode));
                let mut send = |object: u32, opcode: u16, body: &[u8]| {
                    let size = 8 + body.len() as u32;
                    let mut m = object.to_ne_bytes().to_vec();
                    m.extend(((size << 16) | u32::from(opcode)).to_ne_bytes());
                    m.extend(body);
                    s.write_all(&m).expect("event");
                };
                match (object, opcode) {
                    (1, 0) => send(word(&body, 0).expect("id"), 0, &[0; 4]),
                    (1, 1) => {
                        for (name, (iface, version)) in (1u32..).zip(&globals) {
                            let mut g = name.to_ne_bytes().to_vec();
                            let len = iface.len() + 1;
                            g.extend((len as u32).to_ne_bytes());
                            g.extend(iface.as_bytes());
                            g.resize(g.len() + len.div_ceil(4) * 4 - iface.len(), 0);
                            g.extend(version.to_ne_bytes());
                            send(word(&body, 0).expect("id"), 0, &g);
                        }
                    }
                    (_, op::DMABUF_CREATE_PARAMS) if params == 0 && object != 1 => {
                        params = word(&body, 0).expect("params id");
                    }
                    (o, op::PARAMS_CREATE) if o == params => {
                        if imports {
                            send(params, op::PARAMS_CREATED, &CREATED.to_ne_bytes());
                        } else {
                            send(params, op::PARAMS_FAILED, &[]);
                        }
                    }
                    _ => {}
                }
            }
        })
    }

    fn buffer() -> Buffer {
        let (fd, _keep) = std::io::pipe().expect("pipe");
        Buffer {
            fd: fd.into(),
            width: 64,
            height: 64,
            format: ARGB8888,
            offset: 0,
            stride: 256,
            modifier: 0,
        }
    }

    fn ask(globals: Vec<(&'static str, u32)>, imports: bool) -> (Answer, Vec<(u32, u16)>, usize) {
        let (a, b) = UnixStream::pair().expect("pair");
        let server = compositor(b, globals, imports);
        let answer = import(a, buffer()).expect("probe");
        let (seen, fds) = server.join().expect("compositor");
        (answer, seen, fds)
    }

    /// `failed` is a refusal; the buffer went with its fd, and through `create`, never `create_immed` (3), whose refusal would end the connection.
    #[test]
    fn failed_is_refused() {
        let (answer, seen, fds) = ask(vec![(DMABUF, 4)], false);
        assert_eq!(
            answer,
            Answer::Refused("64x64 fourcc 0x34325241 stride 256 modifier 0x0".to_owned())
        );
        assert!(answer.line().starts_with(REFUSED_PREFIX));
        assert_eq!(fds, 1, "the add carries the buffer's fd");
        assert!(seen.iter().any(|&(_, o)| o == op::PARAMS_CREATE));
        assert!(
            !seen.iter().any(|&(o, op)| o > 1 && op == 3),
            "create_immed sent"
        );
    }

    /// `created` is an import, and the made buffer is destroyed again.
    #[test]
    fn created_is_imported() {
        let (answer, seen, _) = ask(vec![("wl_shm", 1), (DMABUF, 3)], true);
        assert!(matches!(answer, Answer::Imported(_)), "{answer:?}");
        assert!(seen.contains(&(CREATED, op::BUFFER_DESTROY)));
    }

    /// No linux-dmabuf global, or one older than `create`'s events: nothing asked, not a refusal.
    #[test]
    fn no_global_is_not_probed() {
        for globals in [vec![("wl_shm", 1)], vec![(DMABUF, 2)]] {
            let (answer, ..) = ask(globals, false);
            assert!(matches!(answer, Answer::NotProbed(_)), "{answer:?}");
        }
    }

    /// No render node: not probed, named.
    #[test]
    fn no_render_node_is_not_probed() {
        let dir = std::env::temp_dir().join(format!("atl-dmabuf-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let err = allocate(&dir).err().expect("no node");
        assert!(format!("{err:#}").contains("no render node"), "{err:#}");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
