// SPDX-License-Identifier: MIT
// Adapters from the cxx bridge to AOSP libinput's public C++ API (see input.h).

#include "wrap/input.h"

#include <android-base/unique_fd.h>
#include <android/input.h>
#include <binder/Parcel.h>
#include <binder/ParcelFileDescriptor.h>
#include <ui/LogicalDisplayId.h>
#include <ui/Transform.h>

#include <string>
#include <utility>
#include <variant>
#include <vector>

namespace atl::input {

namespace {

// The InputChannel over one end of a socket pair: InputChannelCore with the name and the fd, no binder token (the publish gate has no server).
std::shared_ptr<android::InputChannel> open_channel(int32_t fd, rust::Str name) {
    android::os::InputChannelCore core;
    core.name = std::string(name.data(), name.size());
    core.fd = android::os::ParcelFileDescriptor(android::base::unique_fd(fd));
    return std::shared_ptr<android::InputChannel>(android::InputChannel::create(std::move(core)));
}

}  // namespace

Publisher::Publisher(std::shared_ptr<android::InputChannel> channel) : publisher(std::move(channel)) {}

Consumer::Consumer(std::shared_ptr<android::InputChannel> channel) : consumer(std::move(channel)) {}

KeyCharacterMap::KeyCharacterMap(std::shared_ptr<android::KeyCharacterMap> map) : map(std::move(map)) {}

KeyLayoutMap::KeyLayoutMap(std::shared_ptr<android::KeyLayoutMap> map) : map(std::move(map)) {}

std::unique_ptr<Message> new_message() { return std::make_unique<Message>(); }

std::unique_ptr<Publisher> new_publisher(int32_t fd, rust::Str name) {
    auto channel = open_channel(fd, name);
    if (!channel) {
        return nullptr;
    }
    return std::make_unique<Publisher>(std::move(channel));
}

int32_t publish_key(Publisher& publisher, uint32_t seq, int32_t event_id, int32_t device_id, int32_t source, int32_t display_id, int32_t action, int32_t flags, int32_t key_code, int32_t scan_code, int32_t meta_state, int32_t repeat_count, int64_t down_time, int64_t event_time) {
    return publisher.publisher.publishKeyEvent(seq, event_id, device_id, source, android::ui::LogicalDisplayId(display_id), {}, action, flags, key_code, scan_code, meta_state, repeat_count, down_time, event_time);
}

int32_t publish_motion(Publisher& publisher, uint32_t seq, int32_t event_id, int32_t device_id, int32_t source, int32_t display_id, int32_t action, int32_t action_button, int32_t flags, int32_t edge_flags, int32_t meta_state, int32_t button_state, uint8_t classification, rust::Slice<const float> transform, float x_precision, float y_precision, float x_cursor, float y_cursor, int64_t down_time, int64_t event_time, rust::Slice<const int32_t> pointer_ids, rust::Slice<const int32_t> tool_types, rust::Slice<const uint32_t> axis_pointers, rust::Slice<const int32_t> axis_ids, rust::Slice<const float> axis_values) {
    android::ui::Transform affine;
    affine.set(transform[0], transform[1], transform[2], transform[3]);
    affine.set(transform[4], transform[5]);
    const size_t count = pointer_ids.size();
    std::vector<android::PointerProperties> properties(count);
    std::vector<android::PointerCoords> pointer_coords(count);
    for (size_t i = 0; i < count; i++) {
        properties[i].clear();
        properties[i].id = pointer_ids[i];
        properties[i].toolType = static_cast<android::ToolType>(tool_types[i]);
        pointer_coords[i].clear();
    }
    for (size_t k = 0; k < axis_ids.size(); k++) {
        pointer_coords[axis_pointers[k]].setAxisValue(axis_ids[k], axis_values[k]);
    }
    return publisher.publisher.publishMotionEvent(seq, event_id, device_id, source, android::ui::LogicalDisplayId(display_id), {}, action, action_button, flags, edge_flags, meta_state, button_state, static_cast<android::MotionClassification>(classification), affine, x_precision, y_precision, x_cursor, y_cursor, affine, down_time, event_time, static_cast<uint32_t>(count), properties.data(), pointer_coords.data());
}

int32_t publish_focus(Publisher& publisher, uint32_t seq, int32_t event_id, bool has_focus) {
    return publisher.publisher.publishFocusEvent(seq, event_id, has_focus);
}

int32_t receive_finished(Publisher& publisher, Message& out) {
    auto response = publisher.publisher.receiveConsumerResponse();
    if (!response.ok()) {
        return response.error().code();
    }
    if (const auto* finished = std::get_if<android::InputPublisher::Finished>(&response.value())) {
        out.kind_ = 3;
        out.seq_ = finished->seq;
        out.handled_ = finished->handled;
    } else {
        out.kind_ = 4;
    }
    return 0;
}

std::unique_ptr<Consumer> new_consumer(int32_t fd, rust::Str name) {
    auto channel = open_channel(fd, name);
    if (!channel) {
        return nullptr;
    }
    return std::make_unique<Consumer>(std::move(channel));
}

int32_t consume(Consumer& consumer, Message& out) {
    android::PreallocatedInputEventFactory factory;
    uint32_t seq = 0;
    android::InputEvent* event = nullptr;
    const int32_t status = consumer.consumer.consume(&factory, /*consumeBatches=*/true, /*frameTime=*/-1, &seq, &event);
    if (status != 0) {
        return status;
    }
    out.seq_ = seq;
    if (event == nullptr) {
        out.kind_ = 4;
    } else if (event->getType() == android::InputEventType::KEY) {
        const auto* key = static_cast<const android::KeyEvent*>(event);
        out.kind_ = 1;
        out.action_ = key->getAction();
        out.key_code_ = key->getKeyCode();
        out.device_id_ = key->getDeviceId();
    } else if (event->getType() == android::InputEventType::MOTION) {
        const auto* motion = static_cast<const android::MotionEvent*>(event);
        out.kind_ = 2;
        out.action_ = motion->getAction();
        out.device_id_ = motion->getDeviceId();
        out.pointer_id_ = motion->getPointerId(0);
        out.x_ = motion->getX(0);
        out.y_ = motion->getY(0);
        out.tool_type_ = static_cast<int32_t>(motion->getToolType(0));
        out.vscroll_ = motion->getAxisValue(AMOTION_EVENT_AXIS_VSCROLL, 0);
    } else if (event->getType() == android::InputEventType::FOCUS) {
        out.kind_ = 5;
        out.has_focus_ = static_cast<const android::FocusEvent*>(event)->getHasFocus();
    } else {
        out.kind_ = 4;
    }
    return 0;
}

int32_t finish(Consumer& consumer, uint32_t seq, bool handled) {
    return consumer.consumer.sendFinishedSignal(seq, handled);
}

std::unique_ptr<KeyCharacterMap> load_kcm(rust::Str file_name, rust::Slice<const uint8_t> contents) {
    // Tokenizer::fromContents measures the text with strlen: hand it a NUL-terminated copy
    const std::string text(reinterpret_cast<const char*>(contents.data()), contents.size());
    auto loaded = android::KeyCharacterMap::loadContents(std::string(file_name.data(), file_name.size()), text.c_str(), android::KeyCharacterMap::Format::ANY);
    if (!loaded.ok()) {
        return nullptr;
    }
    return std::make_unique<KeyCharacterMap>(loaded.value());
}

rust::Vec<uint8_t> kcm_parcel_bytes(const KeyCharacterMap& map) {
    android::Parcel parcel;
    map.map->writeToParcel(&parcel);
    const auto* data = reinterpret_cast<const uint8_t*>(parcel.data());
    rust::Vec<uint8_t> bytes;
    bytes.reserve(parcel.dataSize());
    for (size_t i = 0; i < parcel.dataSize(); i++) {
        bytes.push_back(data[i]);
    }
    return bytes;
}

std::unique_ptr<KeyLayoutMap> load_key_layout(rust::Str path) {
    auto loaded = android::KeyLayoutMap::load(std::string(path.data(), path.size()));
    if (!loaded.ok()) {
        return nullptr;
    }
    return std::make_unique<KeyLayoutMap>(loaded.value());
}

int32_t map_key(const KeyLayoutMap& map, int32_t scan_code, int32_t& key_code, uint32_t& flags) {
    return map.map->mapKey(scan_code, 0, &key_code, &flags);
}

int32_t update_meta_state(int32_t key_code, bool down, int32_t old_meta_state) {
    return android::updateMetaState(key_code, down, old_meta_state);
}

int32_t normalize_meta_state(int32_t meta_state) { return android::normalizeMetaState(meta_state); }

}  // namespace atl::input
