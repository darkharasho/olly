import QtQuick
import org.kde.ksvg as KSvg
import org.kde.kirigami as Kirigami

// olly's screen face for one mood. Blinks in calm, pulses a "?" in needs.
Item {
    id: face

    property string mood: "offline"
    property bool blinking: false
    property bool updateReady: false
    readonly property bool animate: Kirigami.Units.longDuration > 0

    implicitWidth: Kirigami.Units.iconSizes.medium
    implicitHeight: implicitWidth

    KSvg.SvgItem {
        id: svg
        anchors.centerIn: parent
        width: Math.min(face.width, face.height)
        height: width
        imagePath: Qt.resolvedUrl("../images/face-"
            + (face.mood === "calm" && face.blinking ? "calm-blink" : face.mood) + ".svg")
    }

    Timer {
        interval: 5000
        repeat: true
        running: face.animate && face.mood === "calm"
        onTriggered: {
            face.blinking = true;
            unblink.restart();
        }
        onRunningChanged: if (!running) face.blinking = false
    }

    Timer {
        id: unblink
        interval: 150
        onTriggered: face.blinking = false
    }

    // Placed where the mockup's "?" sits in the 24x24 viewBox: x 17.6, baseline 9.4.
    Text {
        id: question
        visible: face.mood === "needs"
        text: "?"
        color: "#ffa726"
        font.pixelSize: Math.max(1, svg.height * 5 / 24)
        font.weight: Font.ExtraBold
        x: svg.x + svg.width * 17.6 / 24
        y: svg.y + svg.height * 9.4 / 24 - baselineOffset

        SequentialAnimation on opacity {
            running: face.animate && face.mood === "needs"
            loops: Animation.Infinite
            NumberAnimation { to: 0.25; duration: 600; easing.type: Easing.InOutSine }
            NumberAnimation { to: 1; duration: 600; easing.type: Easing.InOutSine }
            onRunningChanged: if (!running) question.opacity = 1
        }
    }

    // Antenna glow: the tip at cx 12, cy 2.6, r 1.2 in the 24x24 viewBox.
    Rectangle {
        visible: face.updateReady
        width: svg.width * 2.4 / 24
        height: width
        radius: width / 2
        color: "#4fc3f7"
        x: svg.x + svg.width * 12 / 24 - width / 2
        y: svg.y + svg.height * 2.6 / 24 - height / 2
    }
}
