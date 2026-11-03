import Toybox.BluetoothLowEnergy;
import Toybox.Lang;

// The GATT service the GARMIN node advertises. Must match GARMIN_SERVICE_UUID
// and GARMIN_DATA_CHAR_UUID in rust/src/garmin_node.rs.
module ThalamusProfile {
    function serviceUuid() as BluetoothLowEnergy.Uuid {
        return BluetoothLowEnergy.stringToUuid("7a4b0001-5c2e-4f6a-9d3b-8e1f2a6c4d90");
    }

    function dataUuid() as BluetoothLowEnergy.Uuid {
        return BluetoothLowEnergy.stringToUuid("7a4b0002-5c2e-4f6a-9d3b-8e1f2a6c4d90");
    }

    function definition() as Dictionary {
        return {
            :uuid => serviceUuid(),
            :characteristics => [{ :uuid => dataUuid() }]
        };
    }
}
