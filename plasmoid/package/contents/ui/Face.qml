import QtQuick
import org.kde.ksvg as KSvg
import org.kde.kirigami as Kirigami

// olly's screen face for one mood, in layers so the parts can move: the body, the antenna,
// and the eyes. Every animation is short and occasional, except while olly is acting or
// napping, and none run when the desktop turns animations off.
Item {
    id: face

    property string mood: "offline"
    property bool updateReady: false
    readonly property bool animate: Kirigami.Units.longDuration > 0

    implicitWidth: Kirigami.Units.iconSizes.medium
    implicitHeight: implicitWidth

    // One unit of the 24x24 viewBox, in pixels.
    readonly property real u: Math.min(width, height) / 24

    // Animated in viewBox units (or degrees, or a factor); the transforms scale them.
    property real lookX: 0
    property real blink: 1
    property real hopY: 0
    property real shakeX: 0
    property real tilt: 0
    property real wiggle: 0
    property real pop: 1
    property real breathe: 1

    property string previousMood: mood

    onMoodChanged: {
        if (animate) {
            if (mood === "onit") {
                popAnim.restart();
            } else if (mood === "calm"
                    && (previousMood === "onit" || previousMood === "worried" || previousMood === "needs")) {
                // Phew.
                reliefAnim.restart();
            }
        }
        previousMood = mood;
    }

    function side() {
        return Math.random() < 0.5 ? -1 : 1;
    }

    function look() {
        lookAnim.side = side();
        lookAnim.restart();
    }

    function tiltHead() {
        tiltAnim.side = side();
        tiltAnim.restart();
    }

    // A little thing to do now and then, picked by mood.
    function idleAct() {
        const r = Math.random();
        if (mood === "calm") {
            if (r < 0.36) blinkAnim.restart();
            else if (r < 0.50) doubleBlinkAnim.restart();
            else if (r < 0.66) look();
            else if (r < 0.78) hopAnim.restart();
            else if (r < 0.89) tiltHead();
            else wiggleAnim.restart();
        } else if (mood === "watch") {
            if (r < 0.45) blinkAnim.restart();
            else scanAnim.restart();
        } else if (mood === "worried") {
            if (r < 0.35) blinkAnim.restart();
            else if (r < 0.70) shiverAnim.restart();
            else sweatAnim.restart();
        } else if (mood === "needs") {
            if (r < 0.5) tiltHead();
            else blinkAnim.restart();
        }
    }

    Timer {
        id: idle
        interval: 4000
        repeat: true
        running: face.animate && ["calm", "watch", "worried", "needs"].indexOf(face.mood) >= 0
        onTriggered: {
            face.idleAct();
            interval = (face.mood === "worried" ? 2500 : 3500) + Math.random() * 6000;
        }
    }

    Item {
        id: screen
        anchors.centerIn: parent
        width: face.u * 24
        height: width
        opacity: face.mood === "offline" ? 0.7 : 1

        transform: [
            Scale {
                origin.x: screen.width / 2
                origin.y: face.u * 20.4
                xScale: face.pop
                yScale: face.pop * face.breathe
            },
            Rotation {
                origin.x: screen.width / 2
                origin.y: face.u * 20.4
                angle: face.tilt
            },
            Translate {
                x: face.shakeX * face.u
                y: face.hopY * face.u
            }
        ]

        // Antenna: stem and tip, wiggling from where the stem meets the head.
        Item {
            id: antenna
            anchors.fill: parent
            transform: Rotation {
                origin.x: face.u * 12
                origin.y: face.u * 5.4
                angle: face.wiggle
            }

            Rectangle {
                width: face.u * 1.3
                height: face.u * 3
                radius: width / 2
                x: face.u * 12 - width / 2
                y: face.u * 2.9
                color: Kirigami.Theme.textColor
                opacity: 0.65
            }
            Rectangle {
                id: tip
                width: face.u * 2.4
                height: width
                radius: width / 2
                x: face.u * 12 - width / 2
                y: face.u * 2.6 - height / 2
                color: Kirigami.Theme.textColor
                opacity: 0.65
            }
            // Update-ready glow, breathing a few times then resting.
            Rectangle {
                id: glow
                visible: face.updateReady
                anchors.fill: tip
                radius: tip.radius
                color: "#4fc3f7"

                SequentialAnimation on opacity {
                    running: face.animate && face.updateReady
                    loops: Animation.Infinite
                    NumberAnimation { to: 0.45; duration: 700; easing.type: Easing.InOutSine }
                    NumberAnimation { to: 1; duration: 700; easing.type: Easing.InOutSine }
                    NumberAnimation { to: 0.45; duration: 700; easing.type: Easing.InOutSine }
                    NumberAnimation { to: 1; duration: 700; easing.type: Easing.InOutSine }
                    PauseAnimation { duration: 6000 }
                    onRunningChanged: if (!running) glow.opacity = 1
                }
            }
            // Acting: the tip flashes red like a little siren.
            Rectangle {
                id: siren
                anchors.fill: tip
                radius: tip.radius
                color: "#ff5252"
                opacity: 0

                SequentialAnimation on opacity {
                    running: face.animate && face.mood === "onit"
                    loops: Animation.Infinite
                    NumberAnimation { to: 1; duration: 180 }
                    PauseAnimation { duration: 220 }
                    NumberAnimation { to: 0; duration: 180 }
                    PauseAnimation { duration: 220 }
                    onRunningChanged: if (!running) siren.opacity = 0
                }
            }
        }

        KSvg.SvgItem {
            anchors.fill: parent
            imagePath: Qt.resolvedUrl("../images/face-body.svg")
        }

        KSvg.SvgItem {
            id: eyes
            anchors.fill: parent
            imagePath: Qt.resolvedUrl("../images/eyes-" + face.mood + ".svg")
            transform: [
                Scale {
                    origin.y: face.u * 12.6
                    yScale: face.blink
                },
                Translate {
                    x: face.lookX * face.u
                }
            ]
        }

        // Placed where the mockup's "?" sits: x 17.6, baseline 9.4.
        Text {
            id: question
            visible: face.mood === "needs"
            text: "?"
            color: "#ffa726"
            font.pixelSize: Math.max(1, face.u * 5)
            font.weight: Font.ExtraBold
            x: face.u * 17.6
            y: face.u * 9.4 - baselineOffset

            SequentialAnimation on opacity {
                running: face.animate && face.mood === "needs"
                loops: Animation.Infinite
                NumberAnimation { to: 0.25; duration: 600; easing.type: Easing.InOutSine }
                NumberAnimation { to: 1; duration: 600; easing.type: Easing.InOutSine }
                onRunningChanged: if (!running) question.opacity = 1
            }
        }

        // Napping: a "z" drifts up and away, over and over.
        Text {
            id: zee
            visible: face.mood === "nap"
            text: "z"
            color: "#9575cd"
            font.pixelSize: Math.max(1, face.u * 4.4)
            font.weight: Font.ExtraBold
            property real rise: 0
            x: face.u * (17.4 + rise * 0.4)
            y: face.u * (9.6 - rise) - baselineOffset

            SequentialAnimation {
                running: face.animate && face.mood === "nap"
                loops: Animation.Infinite
                ParallelAnimation {
                    NumberAnimation { target: zee; property: "rise"; from: 0; to: 3; duration: 2200; easing.type: Easing.OutSine }
                    SequentialAnimation {
                        NumberAnimation { target: zee; property: "opacity"; from: 0; to: 1; duration: 500 }
                        PauseAnimation { duration: 1000 }
                        NumberAnimation { target: zee; property: "opacity"; to: 0; duration: 700 }
                    }
                }
                PauseAnimation { duration: 700 }
                onRunningChanged: if (!running) { zee.rise = 0; zee.opacity = 1; }
            }
        }

        // Worried: a sweat drop slides down the side of the screen.
        Rectangle {
            id: sweat
            property real fall: 0
            width: face.u * 1.5
            height: face.u * 1.9
            radius: width / 2
            color: "#4fc3f7"
            opacity: 0
            x: face.u * 18.6
            y: face.u * (6.6 + fall)
        }
    }

    // ---- one-shot moves ----

    SequentialAnimation {
        id: blinkAnim
        NumberAnimation { target: face; property: "blink"; to: 0.1; duration: 70 }
        NumberAnimation { target: face; property: "blink"; to: 1; duration: 90 }
    }

    SequentialAnimation {
        id: doubleBlinkAnim
        NumberAnimation { target: face; property: "blink"; to: 0.1; duration: 70 }
        NumberAnimation { target: face; property: "blink"; to: 1; duration: 80 }
        PauseAnimation { duration: 120 }
        NumberAnimation { target: face; property: "blink"; to: 0.1; duration: 70 }
        NumberAnimation { target: face; property: "blink"; to: 1; duration: 90 }
    }

    // Glances one way, then the other, then back.
    SequentialAnimation {
        id: lookAnim
        property real side: 1
        NumberAnimation { target: face; property: "lookX"; to: 1.1 * lookAnim.side; duration: 180; easing.type: Easing.OutCubic }
        PauseAnimation { duration: 700 }
        NumberAnimation { target: face; property: "lookX"; to: -1.1 * lookAnim.side; duration: 260; easing.type: Easing.InOutCubic }
        PauseAnimation { duration: 600 }
        NumberAnimation { target: face; property: "lookX"; to: 0; duration: 200; easing.type: Easing.OutCubic }
    }

    // Watching: a slower, careful sweep.
    SequentialAnimation {
        id: scanAnim
        NumberAnimation { target: face; property: "lookX"; to: -1.2; duration: 500; easing.type: Easing.InOutSine }
        NumberAnimation { target: face; property: "lookX"; to: 1.2; duration: 1000; easing.type: Easing.InOutSine }
        NumberAnimation { target: face; property: "lookX"; to: 0; duration: 500; easing.type: Easing.InOutSine }
    }

    SequentialAnimation {
        id: hopAnim
        NumberAnimation { target: face; property: "hopY"; to: -1.6; duration: 160; easing.type: Easing.OutQuad }
        NumberAnimation { target: face; property: "hopY"; to: 0; duration: 260; easing.type: Easing.OutBounce }
    }

    // Head tilt, the curious kind.
    SequentialAnimation {
        id: tiltAnim
        property real side: 1
        NumberAnimation { target: face; property: "tilt"; to: 9 * tiltAnim.side; duration: 220; easing.type: Easing.OutBack }
        PauseAnimation { duration: 900 }
        NumberAnimation { target: face; property: "tilt"; to: 0; duration: 260; easing.type: Easing.InOutQuad }
    }

    SequentialAnimation {
        id: wiggleAnim
        NumberAnimation { target: face; property: "wiggle"; to: 16; duration: 110 }
        NumberAnimation { target: face; property: "wiggle"; to: -13; duration: 150 }
        NumberAnimation { target: face; property: "wiggle"; to: 8; duration: 130 }
        NumberAnimation { target: face; property: "wiggle"; to: -4; duration: 110 }
        NumberAnimation { target: face; property: "wiggle"; to: 0; duration: 100 }
    }

    SequentialAnimation {
        id: shiverAnim
        loops: 3
        NumberAnimation { target: face; property: "shakeX"; to: 0.45; duration: 40 }
        NumberAnimation { target: face; property: "shakeX"; to: -0.45; duration: 80 }
        NumberAnimation { target: face; property: "shakeX"; to: 0; duration: 40 }
    }

    SequentialAnimation {
        id: sweatAnim
        PropertyAction { target: sweat; property: "fall"; value: 0 }
        NumberAnimation { target: sweat; property: "opacity"; to: 0.9; duration: 200 }
        NumberAnimation { target: sweat; property: "fall"; to: 4; duration: 900; easing.type: Easing.InQuad }
        NumberAnimation { target: sweat; property: "opacity"; to: 0; duration: 200 }
    }

    // Springs to attention when it starts acting.
    SequentialAnimation {
        id: popAnim
        NumberAnimation { target: face; property: "pop"; to: 1.18; duration: 120; easing.type: Easing.OutQuad }
        NumberAnimation { target: face; property: "pop"; to: 1; duration: 380; easing.type: Easing.OutElastic }
    }

    // Back to calm: a happy hop and a blink.
    SequentialAnimation {
        id: reliefAnim
        NumberAnimation { target: face; property: "hopY"; to: -2; duration: 180; easing.type: Easing.OutQuad }
        NumberAnimation { target: face; property: "hopY"; to: 0; duration: 320; easing.type: Easing.OutBounce }
        NumberAnimation { target: face; property: "blink"; to: 0.1; duration: 70 }
        NumberAnimation { target: face; property: "blink"; to: 1; duration: 90 }
    }

    // ---- loops while a mood lasts ----

    // Acting: quick, busy little bounces.
    SequentialAnimation {
        running: face.animate && face.mood === "onit"
        loops: Animation.Infinite
        PauseAnimation { duration: 500 }
        NumberAnimation { target: face; property: "hopY"; to: -0.9; duration: 110; easing.type: Easing.OutQuad }
        NumberAnimation { target: face; property: "hopY"; to: 0; duration: 160; easing.type: Easing.InQuad }
        NumberAnimation { target: face; property: "hopY"; to: -0.6; duration: 100; easing.type: Easing.OutQuad }
        NumberAnimation { target: face; property: "hopY"; to: 0; duration: 140; easing.type: Easing.InQuad }
        onRunningChanged: if (!running) face.hopY = 0
    }

    // Napping: slow breaths.
    SequentialAnimation {
        running: face.animate && face.mood === "nap"
        loops: Animation.Infinite
        NumberAnimation { target: face; property: "breathe"; to: 1.035; duration: 1600; easing.type: Easing.InOutSine }
        NumberAnimation { target: face; property: "breathe"; to: 1; duration: 1600; easing.type: Easing.InOutSine }
        onRunningChanged: if (!running) face.breathe = 1
    }
}
