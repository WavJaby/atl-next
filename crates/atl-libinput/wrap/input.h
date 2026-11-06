// SPDX-License-Identifier: MIT
// Adapters from the cxx bridge (src/ffi.rs) to AOSP libinput's public C++ API. Each function converts scalars and slices to AOSP types and forwards one call; no logic (project CLAUDE.md, C++ exception).
#pragma once

#include <cstdint>
#include <memory>

#include "rust/cxx.h"

#include <input/InputConsumer.h>
#include <input/InputTransport.h>
#include <input/KeyCharacterMap.h>
#include <input/KeyLayoutMap.h>
#include <input/Keyboard.h>

namespace atl::input {

// The publisher end: AOSP's InputPublisher over the InputChannel it owns.
class Publisher {
public:
    explicit Publisher(std::shared_ptr<android::InputChannel> channel);

    android::InputPublisher publisher;
};

// The consumer end: AOSP's InputConsumer over the InputChannel it owns.
class Consumer {
public:
    explicit Consumer(std::shared_ptr<android::InputChannel> channel);

    android::InputConsumer consumer;
};

// One received message, filled by consume() or receive_finished(); Rust reads it through the accessors.
class Message {
public:
    int32_t kind() const { return kind_; }
    uint32_t seq() const { return seq_; }
    bool handled() const { return handled_; }
    int32_t action() const { return action_; }
    int32_t key_code() const { return key_code_; }
    int32_t device_id() const { return device_id_; }
    int32_t pointer_id() const { return pointer_id_; }
    float x() const { return x_; }
    float y() const { return y_; }
    int32_t tool_type() const { return tool_type_; }
    float vscroll() const { return vscroll_; }
    bool has_focus() const { return has_focus_; }

    int32_t kind_ = 0;
    uint32_t seq_ = 0;
    bool handled_ = false;
    int32_t action_ = 0;
    int32_t key_code_ = 0;
    int32_t device_id_ = 0;
    int32_t pointer_id_ = 0;
    float x_ = 0;
    float y_ = 0;
    int32_t tool_type_ = 0;
    float vscroll_ = 0;
    bool has_focus_ = false;
};

// A KeyCharacterMap as libinput loaded it.
class KeyCharacterMap {
public:
    explicit KeyCharacterMap(std::shared_ptr<android::KeyCharacterMap> map);

    std::shared_ptr<android::KeyCharacterMap> map;
};

// A KeyLayoutMap (.kl) as libinput loaded it.
class KeyLayoutMap {
public:
    explicit KeyLayoutMap(std::shared_ptr<android::KeyLayoutMap> map);

    std::shared_ptr<android::KeyLayoutMap> map;
};

std::unique_ptr<Message> new_message();
// fd: one SEQPACKET end, owned from here on; null when AOSP refuses the channel.
std::unique_ptr<Publisher> new_publisher(int32_t fd, rust::Str name);
int32_t publish_key(Publisher& publisher, uint32_t seq, int32_t event_id, int32_t device_id, int32_t source, int32_t display_id, int32_t action, int32_t flags, int32_t key_code, int32_t scan_code, int32_t meta_state, int32_t repeat_count, int64_t down_time, int64_t event_time);
// transform, raw_transform: a, b, c, d, tx, ty. pointer_ids, tool_types: one per pointer; axis entry k: pointer axis_pointers[k], axis axis_ids[k], value axis_values[k].
int32_t publish_motion(Publisher& publisher, uint32_t seq, int32_t event_id, int32_t device_id, int32_t source, int32_t display_id, int32_t action, int32_t action_button, int32_t flags, int32_t edge_flags, int32_t meta_state, int32_t button_state, uint8_t classification, rust::Slice<const float> transform, rust::Slice<const float> raw_transform, float x_precision, float y_precision, float x_cursor, float y_cursor, int64_t down_time, int64_t event_time, rust::Slice<const int32_t> pointer_ids, rust::Slice<const int32_t> tool_types, rust::Slice<const uint32_t> axis_pointers, rust::Slice<const int32_t> axis_ids, rust::Slice<const float> axis_values);
// InputPublisher::publishFocusEvent: the window gained or lost input focus.
int32_t publish_focus(Publisher& publisher, uint32_t seq, int32_t event_id, bool has_focus);
int32_t receive_finished(Publisher& publisher, Message& out);

std::unique_ptr<Consumer> new_consumer(int32_t fd, rust::Str name);
int32_t consume(Consumer& consumer, Message& out);
int32_t finish(Consumer& consumer, uint32_t seq, bool handled);

std::unique_ptr<KeyCharacterMap> load_kcm(rust::Str file_name, rust::Slice<const uint8_t> contents);
rust::Vec<uint8_t> kcm_parcel_bytes(const KeyCharacterMap& map);

// KeyLayoutMap::load of the file at `path`; null when libinput refuses it.
std::unique_ptr<KeyLayoutMap> load_key_layout(rust::Str path);
// KeyLayoutMap::mapKey(scan_code, usage 0): the status; the keycode and policy flags go to the out parameters.
int32_t map_key(const KeyLayoutMap& map, int32_t scan_code, int32_t& key_code, uint32_t& flags);
// android::updateMetaState (input/Keyboard.h).
int32_t update_meta_state(int32_t key_code, bool down, int32_t old_meta_state);
// android::normalizeMetaState (input/Keyboard.h).
int32_t normalize_meta_state(int32_t meta_state);

}  // namespace atl::input
