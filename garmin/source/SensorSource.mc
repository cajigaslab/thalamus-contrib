import Toybox.Lang;
import Toybox.Sensor;
import Toybox.WatchUi;

// Feeds heart rate (about once a second) and the accelerometer to a BleLink.
class SensorSource {
    // Must match ACCEL_INTERVAL in rust/src/garmin_node.rs.
    const ACCEL_RATE_HZ = 25;

    var heartRate as Number? = null;

    private var _link as BleLink;

    function initialize(link as BleLink) {
        _link = link;
    }

    function start() as Void {
        Sensor.setEnabledSensors([Sensor.SENSOR_HEARTRATE]);
        Sensor.enableSensorEvents(method(:onSensor));
        Sensor.registerSensorDataListener(method(:onSensorData), {
            :period => 1,
            :accelerometer => { :enabled => true, :sampleRate => ACCEL_RATE_HZ }
        });
    }

    function stop() as Void {
        Sensor.unregisterSensorDataListener();
        Sensor.enableSensorEvents(null);
        Sensor.setEnabledSensors([]);
    }

    function onSensor(info as Sensor.Info) as Void {
        heartRate = info.heartRate;
        _link.sendHeartRate(heartRate != null ? heartRate : 0);
        WatchUi.requestUpdate();
    }

    function onSensorData(data as Sensor.SensorData) as Void {
        var accel = data.accelerometerData;
        if (accel == null || accel.x == null || accel.y == null || accel.z == null) {
            return;
        }
        _link.sendAccel(accel.x, accel.y, accel.z);
    }
}
