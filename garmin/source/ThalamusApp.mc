import Toybox.Application;
import Toybox.Lang;
import Toybox.WatchUi;

// Streams heart rate and accelerometer data to Thalamus's GARMIN node over
// BLE while the app is open.
class ThalamusApp extends Application.AppBase {
    private var _link as BleLink;
    private var _sensors as SensorSource;

    function initialize() {
        AppBase.initialize();
        _link = new BleLink();
        _sensors = new SensorSource(_link);
    }

    function onStart(state as Dictionary?) as Void {
        _link.start();
        _sensors.start();
    }

    function onStop(state as Dictionary?) as Void {
        _sensors.stop();
        _link.stop();
    }

    function getInitialView() {
        return [new ThalamusView(_link, _sensors)];
    }
}
