import Toybox.Graphics;
import Toybox.Lang;
import Toybox.WatchUi;

class ThalamusView extends WatchUi.View {
    private var _link as BleLink;
    private var _sensors as SensorSource;

    function initialize(link as BleLink, sensors as SensorSource) {
        View.initialize();
        _link = link;
        _sensors = sensors;
    }

    function onUpdate(dc as Graphics.Dc) as Void {
        dc.setColor(Graphics.COLOR_WHITE, Graphics.COLOR_BLACK);
        dc.clear();

        var heartRate = _sensors.heartRate;
        var lines = [
            "Thalamus",
            _link.status,
            "HR " + (heartRate != null ? heartRate.toString() : "--"),
            "Sent " + _link.sent,
            "Dropped " + (_link.dropped + _link.failed)
        ];

        var font = Graphics.FONT_SMALL;
        var lineHeight = dc.getFontHeight(font);
        var top = (dc.getHeight() - lineHeight * lines.size()) / 2;
        for (var i = 0; i < lines.size(); i++) {
            dc.drawText(dc.getWidth() / 2, top + i * lineHeight, font, lines[i], Graphics.TEXT_JUSTIFY_CENTER);
        }
    }
}
