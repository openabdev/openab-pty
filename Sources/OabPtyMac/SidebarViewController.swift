import AppKit

/// Outline nodes. `NSOutlineView` needs object identity, and `Profile` /
/// `SessionInfo` are value types, so they are wrapped rather than used directly.
/// Reachability of a connection, as last observed.
///
/// Three states rather than two because "reachable but degraded" is a real and
/// actionable condition here: the runtime can be answering perfectly while
/// reporting leaked processes or a drain in progress, and a client that shows
/// that as plain green is hiding something the operator should see.
enum ConnectionHealth {
    case unknown            // never queried
    case ok                 // answering, nothing flagged
    case degraded(String)   // answering, but something is wrong
    case down(String)       // not answering, or refusing the credential

    var lamp: String {
        switch self {
        case .unknown:  return "○"
        case .ok:       return "●"
        case .degraded: return "●"
        case .down:     return "●"
        }
    }

    var colour: NSColor {
        switch self {
        case .unknown:  return .tertiaryLabelColor
        case .ok:       return .systemGreen
        case .degraded: return .systemYellow
        case .down:     return .systemRed
        }
    }

    var detail: String? {
        switch self {
        case .unknown, .ok: return nil
        case .degraded(let m), .down(let m): return m
        }
    }
}

final class ConnectionNode {
    let profile: Profile
    var sessions: [SessionNode] = []
    var statusLine: String = "not queried yet"
    var health: ConnectionHealth = .unknown
    init(profile: Profile) { self.profile = profile }
}

final class SessionNode {
    let info: SessionInfo
    unowned let connection: ConnectionNode
    init(info: SessionInfo, connection: ConnectionNode) {
        self.info = info
        self.connection = connection
    }
}

protocol SidebarDelegate: AnyObject {
    func sidebar(_ sidebar: SidebarViewController, didChoose session: SessionNode)
    func sidebar(_ sidebar: SidebarViewController, didReportStatus text: String)
    /// The session is gone or its process was replaced, so any pane showing it is
    /// stale and must not keep pretending to be connected.
    func sidebar(_ sidebar: SidebarViewController, didInvalidateSessionNamed name: String)
}

/// Left pane: agent connections, each expanding to its live sessions.
final class SidebarViewController: NSViewController, NSOutlineViewDataSource, NSOutlineViewDelegate {
    weak var delegate: SidebarDelegate?
    private var nodes: [ConnectionNode] = []
    private var outline: NSOutlineView!

    override func loadView() {
        let container = NSView(frame: NSRect(x: 0, y: 0, width: 260, height: 520))

        let addButton = NSButton(title: "＋ Connection", target: self, action: #selector(addConnection))
        addButton.bezelStyle = .rounded
        addButton.translatesAutoresizingMaskIntoConstraints = false

        let newSessionButton = NSButton(title: "＋ Session", target: self, action: #selector(newSession))
        newSessionButton.bezelStyle = .rounded
        newSessionButton.translatesAutoresizingMaskIntoConstraints = false

        let removeButton = NSButton(title: "－", target: self, action: #selector(removeConnection))
        removeButton.bezelStyle = .rounded
        removeButton.translatesAutoresizingMaskIntoConstraints = false

        let refreshButton = NSButton(title: "↻", target: self, action: #selector(refresh))
        refreshButton.bezelStyle = .rounded
        refreshButton.translatesAutoresizingMaskIntoConstraints = false

        let scroll = NSScrollView()
        scroll.translatesAutoresizingMaskIntoConstraints = false
        scroll.hasVerticalScroller = true
        scroll.drawsBackground = false

        outline = NSOutlineView()
        outline.headerView = nil
        outline.rowSizeStyle = .default
        outline.indentationPerLevel = 14
        outline.dataSource = self
        outline.delegate = self
        outline.target = self
        outline.doubleAction = #selector(openSelected)
        let menu = NSMenu()
        menu.delegate = self
        outline.menu = menu
        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("main"))
        column.width = 230
        outline.addTableColumn(column)
        outline.outlineTableColumn = column
        scroll.documentView = outline

        container.addSubview(addButton)
        container.addSubview(newSessionButton)
        container.addSubview(removeButton)
        container.addSubview(refreshButton)
        container.addSubview(scroll)
        NSLayoutConstraint.activate([
            addButton.topAnchor.constraint(equalTo: container.topAnchor, constant: 8),
            addButton.leadingAnchor.constraint(equalTo: container.leadingAnchor, constant: 8),
            newSessionButton.topAnchor.constraint(equalTo: addButton.topAnchor),
            newSessionButton.leadingAnchor.constraint(equalTo: addButton.trailingAnchor, constant: 6),
            refreshButton.topAnchor.constraint(equalTo: addButton.topAnchor),
            removeButton.topAnchor.constraint(equalTo: addButton.topAnchor),
            removeButton.leadingAnchor.constraint(equalTo: newSessionButton.trailingAnchor, constant: 6),
            refreshButton.topAnchor.constraint(equalTo: addButton.topAnchor),
            refreshButton.leadingAnchor.constraint(equalTo: removeButton.trailingAnchor, constant: 6),
            refreshButton.trailingAnchor.constraint(lessThanOrEqualTo: container.trailingAnchor, constant: -8),
            scroll.topAnchor.constraint(equalTo: addButton.bottomAnchor, constant: 8),
            scroll.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: container.trailingAnchor),
            scroll.bottomAnchor.constraint(equalTo: container.bottomAnchor)
        ])
        view = container
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        nodes = ProfileStore.load().map(ConnectionNode.init)
        outline.reloadData()
        refresh()
    }

    private func client(for node: ConnectionNode) -> ApiClient? {
        guard let credential = Keychain.credential(for: node.profile.name) else { return nil }
        return ApiClient(profile: node.profile, credential: credential)
    }

    /// The connection the user actually selected.
    ///
    /// Deliberately returns nil rather than falling back to the first entry:
    /// silently creating a session on a different host than the one the user
    /// meant is worse than refusing, and it is indistinguishable from "create
    /// failed" when the sessions appear somewhere they were not expecting.
    private func selectedConnection() -> ConnectionNode? {
        let row = outline.selectedRow
        guard row >= 0, let item = outline.item(atRow: row) else {
            return nodes.count == 1 ? nodes.first : nil
        }
        if let c = item as? ConnectionNode { return c }
        if let s = item as? SessionNode { return s.connection }
        return nil
    }

    private func complain(_ text: String) {
        delegate?.sidebar(self, didReportStatus: text)
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = text
        alert.runModal()
    }

    // MARK: actions

    @objc func refresh() {  // callable from the window after an attach

        for node in nodes {
            guard let api = client(for: node) else {
                node.statusLine = "credential missing from Keychain"
                node.health = .down("credential missing from Keychain")
                outline.reloadData()
                continue
            }
            Task { @MainActor in
                do {
                    let listing = try await api.list()
                    node.sessions = listing.sessions.map { SessionNode(info: $0, connection: node) }
                    // Report best-effort semantics rather than implying a
                    // guarantee the runtime explicitly does not make.
                    var parts = [listing.killDomain.tier]
                    if listing.killDomain.teardownBestEffort { parts.append("best-effort") }
                    if listing.killDomain.leakedProcesses > 0 {
                        parts.append("⚠︎ \(listing.killDomain.leakedProcesses) leaked")
                    }
                    if listing.draining { parts.append("draining") }
                    node.statusLine = parts.joined(separator: " · ")
                    // Amber for conditions the runtime reports about itself:
                    // a leaked process means Tier 1 teardown did not converge,
                    // and draining means it is going away.
                    if listing.killDomain.leakedProcesses > 0 {
                        node.health = .degraded("\(listing.killDomain.leakedProcesses) leaked process(es)")
                    } else if listing.draining {
                        node.health = .degraded("runtime is draining")
                    } else {
                        node.health = .ok
                    }
                    self.outline.reloadData()
                    self.outline.expandItem(node)
                    self.delegate?.sidebar(self, didReportStatus:
                        "\(node.profile.name): \(listing.sessions.count) session(s) · \(node.statusLine)")
                } catch {
                    node.statusLine = error.localizedDescription
                    // A refused credential is a different problem from an
                    // unreachable host, and the user fixes them differently.
                    if case ApiError.unauthorized = error {
                        node.health = .down("credential rejected")
                    } else {
                        node.health = .down(error.localizedDescription)
                    }
                    node.sessions = []
                    self.outline.reloadData()
                    self.delegate?.sidebar(self, didReportStatus: "\(node.profile.name): \(error.localizedDescription)")
                }
            }
        }
    }

    @objc private func addConnection() {
        let alert = NSAlert()
        alert.messageText = "Add an agent connection"
        alert.informativeText = "The admin credential is kept in your Keychain. It is what lets this app renew its own attach tokens, instead of needing a new one handed to it every few minutes."
        alert.addButton(withTitle: "Save")
        alert.addButton(withTitle: "Cancel")

        let form = NSView(frame: NSRect(x: 0, y: 0, width: 380, height: 92))
        let nameField = NSTextField(frame: NSRect(x: 0, y: 64, width: 380, height: 24))
        nameField.placeholderString = "name (e.g. p1)"
        let urlField = NSTextField(frame: NSRect(x: 0, y: 34, width: 380, height: 24))
        urlField.placeholderString = "base URL (e.g. http://192.168.0.25:8090)"
        let credField = NSSecureTextField(frame: NSRect(x: 0, y: 4, width: 380, height: 24))
        credField.placeholderString = "admin credential"
        form.addSubview(nameField); form.addSubview(urlField); form.addSubview(credField)
        alert.accessoryView = form
        alert.window.initialFirstResponder = nameField

        guard alert.runModal() == .alertFirstButtonReturn else { return }
        let name = nameField.stringValue.trimmingCharacters(in: .whitespaces)
        let url = urlField.stringValue.trimmingCharacters(in: .whitespaces)
        let cred = credField.stringValue.trimmingCharacters(in: .whitespaces)
        guard !name.isEmpty, !url.isEmpty, !cred.isEmpty else {
            delegate?.sidebar(self, didReportStatus: "all three fields are required")
            return
        }
        do { try Keychain.save(credential: cred, for: name) } catch {
            delegate?.sidebar(self, didReportStatus: error.localizedDescription)
            return
        }
        var profiles = ProfileStore.load()
        profiles.removeAll { $0.name == name }
        profiles.append(Profile(baseURL: url, name: name))
        ProfileStore.save(profiles)
        nodes = profiles.map(ConnectionNode.init)
        outline.reloadData()
        refresh()
    }

    @objc private func newSession() {
        guard let node = selectedConnection() else {
            complain("Select a connection in the sidebar first — with more than one configured, this will not guess which host you meant.")
            return
        }
        guard let api = client(for: node) else {
            complain("The credential for \(node.profile.name) is missing from the Keychain. Re-add the connection.")
            return
        }
        let alert = NSAlert()
        alert.messageText = "New session on \(node.profile.name)"
        alert.informativeText = "Names must match [a-z0-9-] and be at most 32 characters."
        let field = NSTextField(frame: NSRect(x: 0, y: 0, width: 260, height: 24))
        field.stringValue = "mac"
        alert.accessoryView = field
        alert.addButton(withTitle: "Create")
        alert.addButton(withTitle: "Cancel")
        guard alert.runModal() == .alertFirstButtonReturn else { return }

        let name = field.stringValue.trimmingCharacters(in: .whitespaces)
        // The runtime's own rule, applied here so the user sees it without a
        // round trip.
        let allowed = name.allSatisfy { ($0.isLetter && $0.isLowercase) || $0.isNumber || $0 == "-" }
        guard !name.isEmpty, name.count <= 32, allowed else {
            delegate?.sidebar(self, didReportStatus: "invalid name: expected [a-z0-9-]{1,32}")
            return
        }
        Task { @MainActor in
            do {
                let grant = try await api.create(name: name)
                let fresh = SessionInfo(name: name, generation: grant.generation, alive: true,
                                        attached: false, bytesWritten: 0,
                                        tier: node.statusLine, teardownBestEffort: true)
                let sessionNode = SessionNode(info: fresh, connection: node)
                node.sessions.append(sessionNode)
                self.outline.reloadData()
                self.outline.expandItem(node)
                self.delegate?.sidebar(self, didChoose: sessionNode)
                self.refresh()
            } catch {
                self.complain("Could not create “\(name)” on \(node.profile.name): \(error.localizedDescription)")
            }
        }
    }

    /// Remove a connection and its Keychain entry. Without this, a changed URL or
    /// rotated credential leaves a dead entry in the list forever.
    @objc private func removeConnection() {
        guard let node = selectedConnection() else {
            complain("Select the connection to remove.")
            return
        }
        let alert = NSAlert()
        alert.messageText = "Remove the connection “\(node.profile.name)”?"
        alert.informativeText = "This forgets the URL and deletes its credential from the Keychain. Sessions on the host are left running."
        alert.addButton(withTitle: "Remove")
        alert.addButton(withTitle: "Cancel")
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        Keychain.delete(profileName: node.profile.name)
        var profiles = ProfileStore.load()
        profiles.removeAll { $0.name == node.profile.name }
        ProfileStore.save(profiles)
        nodes = profiles.map(ConnectionNode.init)
        outline.reloadData()
        refresh()
    }

    @objc private func openSelected() {
        let row = outline.selectedRow
        guard row >= 0, let node = outline.item(atRow: row) as? SessionNode else { return }
        delegate?.sidebar(self, didChoose: node)
    }

    func apiClient(for node: ConnectionNode) -> ApiClient? { client(for: node) }

    // MARK: context menu

    /// The row the menu was opened on, which is not necessarily the selected row.
    private func contextItem() -> Any? {
        let row = outline.clickedRow >= 0 ? outline.clickedRow : outline.selectedRow
        guard row >= 0 else { return nil }
        return outline.item(atRow: row)
    }

    @objc private func menuAttach() {
        guard let s = contextItem() as? SessionNode else { return }
        delegate?.sidebar(self, didChoose: s)
    }

    @objc private func menuRenew() {
        guard let s = contextItem() as? SessionNode, let api = client(for: s.connection) else { return }
        let name = s.info.name
        Task { @MainActor in
            do {
                _ = try await api.renew(name: name)
                // Renew evicts whoever holds the session, by design, so any pane
                // showing it is now stale.
                self.delegate?.sidebar(self, didInvalidateSessionNamed: name)
                self.delegate?.sidebar(self, didReportStatus: "renewed " + name + "; reattach to continue")
                self.refresh()
            } catch {
                self.complain("Renew failed for " + name + ": " + error.localizedDescription)
            }
        }
    }

    @objc private func menuRestart() {
        guard let s = contextItem() as? SessionNode, let api = client(for: s.connection) else { return }
        let name = s.info.name
        if s.info.alive {
            let alert = NSAlert()
            alert.messageText = "Restart the session with a fresh shell?"
            alert.informativeText = "The running process is replaced and its scrollback is discarded. Files in the shared workspace are not touched."
            alert.addButton(withTitle: "Restart")
            alert.addButton(withTitle: "Cancel")
            guard alert.runModal() == .alertFirstButtonReturn else { return }
        }
        Task { @MainActor in
            do {
                _ = try await api.restart(name: name)
                self.delegate?.sidebar(self, didInvalidateSessionNamed: name)
                self.delegate?.sidebar(self, didReportStatus: "restarted " + name + " - attach for a fresh shell")
                self.refresh()
            } catch {
                self.complain("Restart failed for " + name + ": " + error.localizedDescription)
            }
        }
    }

    @objc private func menuTerminate() {
        guard let s = contextItem() as? SessionNode, let api = client(for: s.connection) else { return }
        let name = s.info.name

        // Confirm only when there is something to interrupt. A session whose shell
        // already exited is occupying a slot and nothing else, so asking is noise.
        if s.info.alive {
            let alert = NSAlert()
            alert.alertStyle = .warning
            alert.messageText = "Terminate the session?"
            alert.informativeText = "The running process is killed and its scrollback is lost.\n\n"
                + "Files are not: the workspace is shared between the sessions on this host, "
                + "so anything written there survives. Teardown is best-effort under this kill "
                + "domain, so a process that left its process group may outlive the session."
            alert.addButton(withTitle: "Terminate")
            alert.addButton(withTitle: "Cancel")
            guard alert.runModal() == .alertFirstButtonReturn else { return }
        }
        Task { @MainActor in
            do {
                try await api.kill(name: name)
                self.delegate?.sidebar(self, didInvalidateSessionNamed: name)
                self.delegate?.sidebar(self, didReportStatus: "terminated " + name)
                self.refresh()
            } catch {
                self.complain("Terminate failed for " + name + ": " + error.localizedDescription)
            }
        }
    }

    // MARK: outline data

    func outlineView(_ outlineView: NSOutlineView, numberOfChildrenOfItem item: Any?) -> Int {
        if item == nil { return nodes.count }
        if let c = item as? ConnectionNode { return c.sessions.count }
        return 0
    }

    func outlineView(_ outlineView: NSOutlineView, child index: Int, ofItem item: Any?) -> Any {
        if item == nil { return nodes[index] }
        return (item as! ConnectionNode).sessions[index]
    }

    func outlineView(_ outlineView: NSOutlineView, isItemExpandable item: Any) -> Bool {
        (item as? ConnectionNode) != nil
    }

    func outlineView(_ outlineView: NSOutlineView, viewFor tableColumn: NSTableColumn?, item: Any) -> NSView? {
        let text: String
        let secondary: String
        let lamp: String
        let lampColour: NSColor

        if let c = item as? ConnectionNode {
            text = c.profile.name
            // Show why it is not green, in place: the URL is only useful when
            // everything works, and the reason is what you act on when it does not.
            secondary = c.health.detail ?? c.profile.baseURL
            lamp = c.health.lamp
            lampColour = c.health.colour
        } else if let s = item as? SessionNode {
            text = s.info.name
            secondary = s.info.alive ? (s.info.attached ? "attached" : "detached · alive") : "dead"
            // A session lamp tracks the session, not the host: green while a
            // client holds it, amber when it is alive but nobody is attached,
            // red once the shell is gone.
            lamp = "●"
            lampColour = s.info.alive ? (s.info.attached ? .systemGreen : .systemYellow) : .systemRed
        } else {
            return nil
        }

        let lampField = NSTextField(labelWithString: lamp)
        lampField.font = .systemFont(ofSize: 13)
        lampField.textColor = lampColour
        lampField.setContentHuggingPriority(.required, for: .horizontal)
        // Colour alone is not a signal for everyone who will read this, so the
        // reason is always available as text next to it.
        lampField.toolTip = (item as? ConnectionNode)?.health.detail ?? secondary

        let title = NSTextField(labelWithString: text)
        title.font = (item is ConnectionNode) ? .systemFont(ofSize: 12, weight: .semibold)
                                              : .systemFont(ofSize: 12)
        let sub = NSTextField(labelWithString: secondary)
        sub.font = .systemFont(ofSize: 10)
        sub.textColor = .secondaryLabelColor
        sub.lineBreakMode = .byTruncatingTail

        let labels = NSStackView(views: [title, sub])
        labels.orientation = .vertical
        labels.alignment = .leading
        labels.spacing = 0

        let row = NSStackView(views: [lampField, labels])
        row.orientation = .horizontal
        row.alignment = .centerY
        row.spacing = 6
        return row
    }

    func outlineView(_ outlineView: NSOutlineView, heightOfRowByItem item: Any) -> CGFloat { 34 }
}

extension SidebarViewController: NSMenuDelegate {
    /// Rebuilt on every open so the items match the row clicked, and so a session
    /// whose shell exited offers Restart instead of Attach.
    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        switch contextItem() {
        case let session as SessionNode:
            if session.info.alive {
                menu.addItem(withTitle: "Attach", action: #selector(menuAttach), keyEquivalent: "")
            } else {
                let dead = NSMenuItem(title: "Shell has exited", action: nil, keyEquivalent: "")
                dead.isEnabled = false
                menu.addItem(dead)
            }
            menu.addItem(withTitle: "Restart with a fresh shell", action: #selector(menuRestart), keyEquivalent: "")
            menu.addItem(withTitle: "Renew token (evicts current client)", action: #selector(menuRenew), keyEquivalent: "")
            menu.addItem(.separator())
            menu.addItem(withTitle: session.info.alive ? "Terminate..." : "Terminate",
                         action: #selector(menuTerminate), keyEquivalent: "")
        case is ConnectionNode:
            menu.addItem(withTitle: "New session...", action: #selector(newSession), keyEquivalent: "")
            menu.addItem(withTitle: "Refresh", action: #selector(refresh), keyEquivalent: "")
            menu.addItem(.separator())
            menu.addItem(withTitle: "Remove connection...", action: #selector(removeConnection), keyEquivalent: "")
        default:
            let none = NSMenuItem(title: "No item", action: nil, keyEquivalent: "")
            none.isEnabled = false
            menu.addItem(none)
        }
        for item in menu.items where item.action != nil { item.target = self }
    }
}
