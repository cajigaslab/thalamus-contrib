// Receives data from a device and forwards it to Thalamus over serial, one
// framed message at a time (serial_frame.h). See ../README.md. The only device
// so far is the Garmin app, read by the GARMIN node.
//
// Connect IQ apps can only be BLE centrals, so this is the peripheral: it
// advertises the service the app scans for and the app writes packets to its
// data characteristic, as it would to the GARMIN node. Serial carries nothing
// but frames, so there are no debug prints; status goes out as text packets.
#include <ArduinoBLE.h>

#include "garmin.h"

// Must match the GARMIN node's Baud Rate. Ignored by boards with native USB.
const unsigned long BAUD_RATE = 115200;

BLEService garminService(GARMIN_SERVICE_UUID);
BLECharacteristic garminData(GARMIN_DATA_CHAR_UUID, BLEWrite | BLEWriteWithoutResponse, GARMIN_MAX_PACKET_SIZE);

uint8_t frame[MAX_FRAME_SIZE];

void sendFrame(size_t size) {
  // One write, so a frame is only ever cut short by losing the port. The node
  // skips anything that isn't a whole frame.
  if (size != 0) {
    Serial.write(frame, size);
  }
}

void sendText(const char* text) {
  sendFrame(frameFromGarminText(frame, text));
}

void onConnected(BLEDevice central) {
  char text[48];
  snprintf(text, sizeof(text), "connected to %s", central.address().c_str());
  sendText(text);
}

void onDisconnected(BLEDevice central) {
  char text[48];
  snprintf(text, sizeof(text), "disconnected from %s", central.address().c_str());
  sendText(text);
}

void onGarminPacket(BLEDevice, BLECharacteristic characteristic) {
  sendFrame(writeFrame(frame, characteristic.value(), characteristic.valueLength()));
}

void setup() {
  Serial.begin(BAUD_RATE);
  pinMode(LED_BUILTIN, OUTPUT);

  // Nothing can work without Bluetooth, so keep trying.
  while (!BLE.begin()) {
    sendText("Bluetooth failed to start");
    digitalWrite(LED_BUILTIN, !digitalRead(LED_BUILTIN));
    BLE.end();
    delay(1000);
  }

  BLE.setLocalName("Thalamus");
  BLE.setDeviceName("Thalamus");
  // 15 to 30 ms, in units of 1.25 ms: the watch sends one packet at a time, and
  // its default of 250 ms is far too slow. It can refuse.
  BLE.setConnectionInterval(12, 24);
  BLE.setAdvertisedService(garminService);
  garminService.addCharacteristic(garminData);
  BLE.addService(garminService);
  garminData.setEventHandler(BLEWritten, onGarminPacket);
  BLE.setEventHandler(BLEConnected, onConnected);
  BLE.setEventHandler(BLEDisconnected, onDisconnected);
  BLE.advertise();
}

void loop() {
  static unsigned long last = 0;
  static PinStatus pin = LOW;
  BLE.poll();
  unsigned long now = millis();
  if (now - last > 100) {
    last = now;
    if (BLE.connected()) {
      pin = HIGH;
    } else {
      pin = pin == HIGH ? LOW : HIGH;
    }
    digitalWrite(LED_BUILTIN, pin);
  }
}
