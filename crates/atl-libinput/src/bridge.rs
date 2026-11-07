// SPDX-License-Identifier: MIT
//! The cxx bridge to wrap/input.h. Scalars and flat slices cross; the adapters build AOSP's PointerProperties, PointerCoords and ui::Transform, and fill a `Message` that Rust reads through accessors.
//! Shared structs are not used: a C++ adapter cannot name a record the bridge defines, so every record is spelled out as arguments.

#[cxx::bridge(namespace = "atl::input")]
pub(crate) mod ffi {

    unsafe extern "C++" {
        include!("wrap/input.h");

        /// AOSP InputPublisher over the InputChannel of one SEQPACKET end.
        pub type Publisher;
        /// AOSP InputConsumer over the other end.
        pub type Consumer;
        /// What `consume` and `receive_finished` filled.
        pub type Message;
        /// AOSP KeyCharacterMap.
        pub type KeyCharacterMap;
        /// AOSP KeyLayoutMap.
        pub type KeyLayoutMap;

        pub fn new_message() -> UniquePtr<Message>;
        /// 0 none, 1 key, 2 motion, 3 finished signal, 4 other message.
        pub fn kind(self: &Message) -> i32;
        pub fn seq(self: &Message) -> u32;
        pub fn handled(self: &Message) -> bool;
        pub fn action(self: &Message) -> i32;
        pub fn key_code(self: &Message) -> i32;
        pub fn device_id(self: &Message) -> i32;
        /// The first pointer of a motion event.
        pub fn pointer_id(self: &Message) -> i32;
        pub fn x(self: &Message) -> f32;
        pub fn y(self: &Message) -> f32;
        pub fn tool_type(self: &Message) -> i32;
        pub fn vscroll(self: &Message) -> f32;
        pub fn has_focus(self: &Message) -> bool;

        /// Takes `fd` (one SEQPACKET end) into an InputChannel named `name`; null when AOSP refuses it.
        pub fn new_publisher(fd: i32, name: &str) -> UniquePtr<Publisher>;
        /// InputPublisher::publishKeyEvent with the verifier's HMAC zero.
        pub fn publish_key(
            publisher: Pin<&mut Publisher>,
            seq: u32,
            event_id: i32,
            device_id: i32,
            source: i32,
            display_id: i32,
            action: i32,
            flags: i32,
            key_code: i32,
            scan_code: i32,
            meta_state: i32,
            repeat_count: i32,
            down_time: i64,
            event_time: i64,
        ) -> i32;
        /// InputPublisher::publishMotionEvent. `transform` and `raw_transform` are (a, b, c, d, tx, ty); `pointer_ids` and `tool_types` hold one entry per pointer; axis entry k sets axis `axis_ids[k]` of pointer `axis_pointers[k]` to `axis_values[k]`.
        pub fn publish_motion(
            publisher: Pin<&mut Publisher>,
            seq: u32,
            event_id: i32,
            device_id: i32,
            source: i32,
            display_id: i32,
            action: i32,
            action_button: i32,
            flags: i32,
            edge_flags: i32,
            meta_state: i32,
            button_state: i32,
            classification: u8,
            transform: &[f32],
            raw_transform: &[f32],
            x_precision: f32,
            y_precision: f32,
            x_cursor: f32,
            y_cursor: f32,
            down_time: i64,
            event_time: i64,
            pointer_ids: &[i32],
            tool_types: &[i32],
            axis_pointers: &[u32],
            axis_ids: &[i32],
            axis_values: &[f32],
        ) -> i32;
        /// InputPublisher::publishFocusEvent.
        pub fn publish_focus(
            publisher: Pin<&mut Publisher>,
            seq: u32,
            event_id: i32,
            has_focus: bool,
        ) -> i32;
        /// InputPublisher::receiveConsumerResponse into `out`; the status is WOULD_BLOCK when none is pending.
        pub fn receive_finished(publisher: Pin<&mut Publisher>, out: Pin<&mut Message>) -> i32;
        /// The channel's fd (InputPublisher::getChannel().getFd()); valid while the publisher lives.
        pub fn publisher_fd(publisher: &Publisher) -> i32;

        /// Takes `fd` into an InputChannel for an InputConsumer; null when AOSP refuses it.
        pub fn new_consumer(fd: i32, name: &str) -> UniquePtr<Consumer>;
        /// InputConsumer::consume (batches included) into `out`; the status is WOULD_BLOCK when no message is pending.
        pub fn consume(consumer: Pin<&mut Consumer>, out: Pin<&mut Message>) -> i32;
        /// InputConsumer::sendFinishedSignal.
        pub fn finish(consumer: Pin<&mut Consumer>, seq: u32, handled: bool) -> i32;

        /// KeyCharacterMap::loadContents (format ANY) of `contents`, named `file_name`; null when libinput refuses the map.
        pub fn load_kcm(file_name: &str, contents: &[u8]) -> UniquePtr<KeyCharacterMap>;
        /// KeyCharacterMap::writeToParcel: the Parcel bytes.
        pub fn kcm_parcel_bytes(map: &KeyCharacterMap) -> Vec<u8>;

        /// KeyLayoutMap::load of the file at `path`; null when libinput refuses it.
        pub fn load_key_layout(path: &str) -> UniquePtr<KeyLayoutMap>;
        /// KeyLayoutMap::mapKey(scan_code, usage 0): the status; keycode and policy flags into the out parameters.
        pub fn map_key(
            map: &KeyLayoutMap,
            scan_code: i32,
            key_code: &mut i32,
            flags: &mut u32,
        ) -> i32;
        /// android::updateMetaState.
        pub fn update_meta_state(key_code: i32, down: bool, old_meta_state: i32) -> i32;
        /// android::normalizeMetaState.
        pub fn normalize_meta_state(meta_state: i32) -> i32;
    }
}
