pragma ComponentBehavior: Bound

import QtQuick
import org.kde.plasma.plasmoid
import org.kde.plasma.core as PlasmaCore
import org.kde.plasma.plasma5support as Plasma5Support
import "logic.js" as Logic

// Data layer: polls the olly CLI and hands the state to Face and Popup.
PlasmoidItem {
    id: root

    readonly property string statusCmd: Logic.OLLY + " status --json"
    readonly property string incidentsCmd: Logic.OLLY + " incidents -n 20 --json"

    property var status: null           // StatusData, or null when offline
    property var incidents: []          // kept across offline spells
    property var seenIncidentId: undefined
    property double now: Date.now()
    property bool repoll: false         // an action finished while a poll was in flight
    property string actionError: ""
    property var actionErrorTarget: null // frozen id for card actions, null for the footer

    readonly property string mood: Logic.mood(status, now)

    Plasmoid.status: mood === "needs" ? PlasmaCore.Types.NeedsAttentionStatus : PlasmaCore.Types.ActiveStatus
    toolTipMainText: "olly"
    toolTipSubText: mood === "offline" ? "olly isn't running right now." : Logic.stripBot(status.message || "")

    function connectOnce(cmd) {
        // A source that is still connected is still running: skip instead of piling up.
        if (exec.connectedSources.indexOf(cmd) >= 0)
            return false;
        exec.connectSource(cmd);
        return true;
    }

    // After an action: poll now, or as soon as the poll in flight returns.
    function pollSoon() {
        if (!connectOnce(statusCmd))
            repoll = true;
    }

    function fetchIncidents() {
        connectOnce(incidentsCmd);
    }

    // verb: "kill" | "unfreeze" (with a frozen id from status) | "pause" | "resume".
    function run(verb, id) {
        var cmd = Logic.command(verb, id);
        if (cmd === "")
            return;
        actionErrorTarget = (verb === "kill" || verb === "unfreeze") ? id : null;
        connectOnce(cmd);
    }

    function handleResult(source, code, stdout, stderr) {
        if (source === statusCmd) {
            now = Date.now();
            status = Logic.parseStatus(code, stdout);
            if (status && status.last_incident_id !== seenIncidentId) {
                seenIncidentId = status.last_incident_id;
                fetchIncidents();
            }
            if (repoll) {
                repoll = false;
                connectOnce(statusCmd);
            }
        } else if (source === incidentsCmd) {
            var list = Logic.parseIncidents(code, stdout);
            if (list)
                incidents = list;
        } else {
            if (code === 0) {
                actionError = "";
                errorTimer.stop();
            } else {
                actionError = Logic.stripBot(stderr || "") || "Hmm, that didn't work.";
                errorTimer.restart();
            }
            pollSoon();
        }
    }

    onExpandedChanged: if (expanded) fetchIncidents()

    Plasma5Support.DataSource {
        id: exec
        engine: "executable"
        connectedSources: []
        onNewData: (sourceName, data) => {
            exec.disconnectSource(sourceName);
            root.handleResult(sourceName, data["exit code"], data["stdout"], data["stderr"]);
        }
    }

    Timer {
        interval: 2000
        repeat: true
        running: true
        triggeredOnStart: true
        onTriggered: root.connectOnce(root.statusCmd)
    }

    // Moves countdowns and the 60 s "On it!" linger between polls.
    Timer {
        interval: 1000
        repeat: true
        running: root.mood === "needs" || root.mood === "onit"
        onTriggered: root.now = Date.now()
    }

    Timer {
        id: errorTimer
        interval: 6000
        onTriggered: root.actionError = ""
    }

    compactRepresentation: MouseArea {
        property bool wasExpanded: false
        acceptedButtons: Qt.LeftButton
        onPressed: wasExpanded = root.expanded
        onClicked: root.expanded = !wasExpanded

        Face {
            anchors.fill: parent
            mood: root.mood
        }
    }

    fullRepresentation: Popup {
        app: root
    }
}
