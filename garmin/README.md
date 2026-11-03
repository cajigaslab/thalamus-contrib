# Thalamus Garmin app

A Connect IQ watch app that streams heart rate and 25 Hz
accelerometer, gyroscope and magnetometer data to
Thalamus's `GARMIN` node (`rust/src/garmin_node.rs`) over Bluetooth LE.

Connect IQ apps can only be BLE centrals, so the PC running Thalamus is the
peripheral: the node advertises service `7a4b0001-5c2e-4f6a-9d3b-8e1f2a6c4d90`
and the watch scans for it, connects, and writes packets to characteristic
`7a4b0002-5c2e-4f6a-9d3b-8e1f2a6c4d90`. The packet format is documented in
`source/BleLink.mc` and `parse_packet` in the node.

## Building

Install the Connect IQ SDK and the devices you need with the SDK Manager,
create a developer key, then:

```sh
monkeyc -f monkey.jungle -d fr965 -y developer_key.der -o bin/Thalamus.prg
```

Copy `bin/Thalamus.prg` to the watch's `GARMIN/APPS` folder, or run it in the
simulator (`connectiq`, then `monkeydo bin/Thalamus.prg fr965`). Add devices
to `manifest.xml` as needed.

## Using

1. Add a `GARMIN` node in Thalamus and check `Running`. It needs a Bluetooth
   adapter that supports the peripheral role (most recent Intel adapters on
   Windows do; BlueZ on Linux).
2. Open the Thalamus app on the watch. It shows `Scanning`, then `Connected`.
   Data only flows while the app is open.

The node outputs `HR` (bpm, 1 s interval),
`ACC_X`, `ACC_Y`, `ACC_Z` (g), `GYR_X`, `GYR_Y`, `GYR_Z` (deg/s) and `MAG_X`,
`MAG_Y`, `MAG_Z` (milli-gauss, all at 40 ms intervals), timestamped on arrival.

To use a PC without a suitable adapter, the sketch in `arduino/` takes the
PC's place and forwards the packets over serial: set the node's `Serial Port`.
