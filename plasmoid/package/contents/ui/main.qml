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
    property var seenIncidentId: undefined // last_incident_id the list was fetched for
    property var fetchingIncidentId: undefined // last_incident_id the fetch in flight is for
    property double now: Date.now()
    property bool repoll: false         // an action finished while a poll was in flight
    property double updateDismissedAt: 0 // newest last_update/update_error at_wall_ms seen when the popup closed
    property string actionError: ""
    property var actionErrorTarget: null // frozen id of the failed card action, null for the footer
    property var commandTargets: ({})    // action command -> frozen id (or null), set when connecting
    // Back off while offline so a stalled daemon doesn't collect abandoned connections.
    property int pollInterval: 2000

    readonly property string mood: Logic.mood(status, now)

    Plasmoid.status: mood === "needs" ? PlasmaCore.Types.NeedsAttentionStatus : PlasmaCore.Types.ActiveStatus
    toolTipMainText: "olly"
    toolTipSubText: mood === "offline" ? "olly isn't running right now." : Logic.stripBot((status && status.message) || "")

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

    // `forId` is the last_incident_id this fetch is for. It only becomes
    // seenIncidentId when the fetch succeeds, so a failed one is retried.
    function fetchIncidents(forId) {
        if (connectOnce(incidentsCmd))
            fetchingIncidentId = forId;
    }

    // verb: "kill" | "unfreeze" (with a frozen id from status) | "pause" | "resume" | "update".
    function run(verb, id) {
        var cmd = Logic.command(verb, id);
        if (cmd === "")
            return;
        commandTargets[cmd] = (verb === "kill" || verb === "unfreeze") ? id
            : (verb === "update" ? "update" : null);
        connectOnce(cmd);
    }

    function handleResult(source, code, stdout, stderr) {
        if (source === statusCmd) {
            now = Date.now();
            status = Logic.parseStatus(code, stdout);
            pollInterval = status ? 2000 : 5000;
            if (status && status.last_incident_id !== seenIncidentId)
                fetchIncidents(status.last_incident_id);
            if (repoll) {
                repoll = false;
                connectOnce(statusCmd);
            }
        } else if (source === incidentsCmd) {
            var list = Logic.parseIncidents(code, stdout);
            if (list) {
                incidents = list;
                seenIncidentId = fetchingIncidentId;
            }
        } else {
            var target = commandTargets.hasOwnProperty(source) ? commandTargets[source] : null;
            delete commandTargets[source];
            if (code === 0) {
                actionError = "";
                errorTimer.stop();
            } else {
                actionErrorTarget = target;
                actionError = Logic.stripBot(stderr || "") || "Hmm, that didn't work.";
                errorTimer.restart();
            }
            pollSoon();
        }
    }

    onExpandedChanged: {
        if (expanded)
            fetchIncidents(status ? status.last_incident_id : seenIncidentId);
        else if (status && (status.last_update || status.update_error))
            updateDismissedAt = Logic.updateSeenAt(status);
    }

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
        interval: root.pollInterval
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
            updateReady: Logic.hasUpdate(root.status)
        }
    }

    fullRepresentation: Popup {
        app: root
    }
}
