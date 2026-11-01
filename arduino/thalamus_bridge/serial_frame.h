// Frames messages for serial, read by Deframer in rust/src/garmin_node.rs:
//   byte 0: message length, 1 to MAX_FRAME_PAYLOAD
//   then the message, then 0xa5
//
// This file has no Arduino dependencies, so it can be tested on a PC.
#pragma once

#include <stddef.h>
#include <stdint.h>
#include <string.h>

const uint8_t FRAME_END = 0xa5;
const size_t MAX_FRAME_PAYLOAD = 64;
const size_t MAX_FRAME_SIZE = 2 + MAX_FRAME_PAYLOAD;

// Writes a frame to `frame`, MAX_FRAME_SIZE bytes. Returns its size, 0 if the
// message doesn't fit.
inline size_t writeFrame(uint8_t* frame, const uint8_t* message, size_t size) {
  if (size == 0 || size > MAX_FRAME_PAYLOAD) {
    return 0;
  }
  frame[0] = uint8_t(size);
  memcpy(frame + 1, message, size);
  frame[1 + size] = FRAME_END;
  return 2 + size;
}
