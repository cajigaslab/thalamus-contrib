# Arduino bridge

`thalamus_bridge` receives data from a device and forwards it to Thalamus over
serial. To support another device, add a header like `thalamus_bridge/garmin.h`
and receive its data in the sketch.

## Building

Use a board [ArduinoBLE](https://docs.arduino.cc/libraries/arduinoble/)
supports, such as an UNO R4 WiFi, Nano 33 BLE, Nano 33 IoT or Nano ESP32.
Install ArduinoBLE with the Library Manager, then open
`thalamus_bridge/thalamus_bridge.ino` in the Arduino IDE and upload it, or:

```sh
arduino-cli compile -b arduino:renesas_uno:unor4wifi --upload -p COM3 thalamus_bridge
```

## Format

Each message is framed by `thalamus_bridge/serial_frame.h`: the message length,
the message, then `0xa5`. The reader can find the next frame from anywhere in
the stream by retrying a byte later when a frame doesn't end in `0xa5`. There's
no checksum, so a corrupted value inside a frame isn't detected. Serial only
carries frames, so the sketch sends its status as messages too.

## Garmin

The sketch receives packets from the Connect IQ app in `garmin/` over Bluetooth
LE, so the PC doesn't need a Bluetooth adapter that can be a peripheral.

The sketch is the BLE peripheral the app already looks for: it advertises
service `7a4b0001-5c2e-4f6a-9d3b-8e1f2a6c4d90` and takes the app's packets on
characteristic `7a4b0002-5c2e-4f6a-9d3b-8e1f2a6c4d90`. It forwards them
unchanged, plus text packets (type `0x7f`) for its status.

1. Add a `GARMIN` node in Thalamus. Set `Serial Port` to the board's port
   (`COM3`, `/dev/ttyACM0`), leave the serial fields at their defaults and
   check `Running`.
2. Open the Thalamus app on the watch. It shows `Scanning`, then `Connected`,
   and the board's LED stops blinking. The first time, the watch may tell you
   it's using an open connection, since the link isn't encrypted.

Run either the sketch or the node's Bluetooth mode, since the app connects to
the first it finds. The node's output is described in `garmin/README.md`, and
the sketch's status messages go to Thalamus's log.
