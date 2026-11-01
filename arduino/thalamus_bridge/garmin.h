// What the sketch needs to know about the Garmin app. It forwards the app's
// packets as they are; rust/src/garmin_node.rs parses them.
#pragma once

#include "serial_frame.h"

// Must match garmin/source/ThalamusProfile.mc.
const char GARMIN_SERVICE_UUID[] = "7a4b0001-5c2e-4f6a-9d3b-8e1f2a6c4d90";
const char GARMIN_DATA_CHAR_UUID[] = "7a4b0002-5c2e-4f6a-9d3b-8e1f2a6c4d90";

const size_t GARMIN_MAX_PACKET_SIZE = 20;

// A status message from this sketch, in the packet format: this type, a zero
// where the app has a sequence number, then the text. Must match PACKET_TEXT
// in rust/src/garmin_node.rs.
const uint8_t GARMIN_PACKET_TEXT = 0x7f;

// Writes a frame for a status message, cutting off text that doesn't fit.
inline size_t frameFromGarminText(uint8_t* frame, const char* text) {
  uint8_t message[MAX_FRAME_PAYLOAD] = {GARMIN_PACKET_TEXT, 0};
  size_t size = strlen(text);
  if (size > MAX_FRAME_PAYLOAD - 2) {
    size = MAX_FRAME_PAYLOAD - 2;
  }
  memcpy(message + 2, text, size);
  return writeFrame(frame, message, 2 + size);
}
