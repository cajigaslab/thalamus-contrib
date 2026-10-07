/* A stand-in for the OpenH264 library, for building FFmpeg with
 * --enable-libopenh264 without OpenH264 itself (see openh264.cmake). FFmpeg's
 * configure check and command line tools link against it; every function
 * fails. The Thalamus plugin defines the encoder functions itself, loading
 * Cisco's binary at run time (rust/src/openh264.rs), and isn't linked with
 * this. */
#include <stddef.h>
#include <string.h>

#include "wels/codec_api.h"

int WelsCreateSVCEncoder(ISVCEncoder **ppEncoder) {
  *ppEncoder = NULL;
  return 1;
}

void WelsDestroySVCEncoder(ISVCEncoder *pEncoder) { (void)pEncoder; }

long WelsCreateDecoder(ISVCDecoder **ppDecoder) {
  *ppDecoder = NULL;
  return 1;
}

void WelsDestroyDecoder(ISVCDecoder *pDecoder) { (void)pDecoder; }

OpenH264Version WelsGetCodecVersion(void) {
  OpenH264Version version;
  memset(&version, 0, sizeof(version));
  return version;
}

void WelsGetCodecVersionEx(OpenH264Version *pVersion) { memset(pVersion, 0, sizeof(*pVersion)); }
