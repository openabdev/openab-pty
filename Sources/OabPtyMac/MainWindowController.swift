import AppKit

/// Main window: manage connections, list sessions, open terminals.
final class MainWindowController: NSWindowController, NSTableViewDataSource, NSTableViewDelegate {
    private var profiles: [Profile] = ProfileStore.load()
    private var selectedProfile: Profile? { profilePopUp.indexOfSelectedItem >= 0 && profilePopUp.indexOfSelectedItem < profiles.count ? profiles[profilePopUp.indexOfSelectedItem] : nil }

    private var profilePopUp: NSPopUpButton!
    private var table: NSTableView!
    private var status: NSTextField!
    private var sessions: [SessionInfo] = []
    private var terminals: [TerminalWindowController] = []

    init() {
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 620, height: 420),
                              styleMask: [.titled, .closable, .resizable, .miniaturizable],
                              backing: .buffered, defer: false)
        window.title = "openab-pty"
        super.init(window: window)
        buildUI()
        refreshProfiles()
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    private func buildUI() {
        let content = NSView(frame: window!.contentView!.bounds)
        content.autoresizingMask = [.width, .height]

        profilePopUp = NSPopUpButton(frame: NSRect(x: 12, y: 380, width: 260, height: 25))
        profilePopUp.target = self
        profilePopUp.action = #selector(profileChanged)

        let addButton = NSButton(title: "Add connection…", target: self, action: #selector(addConnection))
        addButton.frame = NSRect(x: 280, y: 379, width: 150, height: 26)

        let refreshButton = NSButton(title: "Refresh", target: self, action: #selector(refreshSessions))
        refreshButton.frame = NSRect(x: 436, y: 379, width: 80, height: 26)

        let newButton = NSButton(title: "New session…", target: self, action: #selector(newSession))
        newButton.frame = NSRect(x: 520, y: 379, width: 88, height: 26)

        let scroll = NSScrollView(frame: NSRect(x: 12, y: 40, width: 596, height: 330))
        scroll.autoresizingMask = [.width, .height]
        scroll.hasVerticalScroller = true
        table = NSTableView(frame: scroll.bounds)
        for (id, title, width) in [("name", "Session", 160), ("state", "State", 150),
                                   ("tier", "Kill domain", 180), ("bytes", "Output", 80)] {
            let col = NSTableColumn(identifier: NSUserInterfaceItemIdentifier(id))
            col.title = title
            col.width = CGFloat(width)
            table.addTableColumn(col)
        }
        table.dataSource = self
        table.delegate = self
        table.doubleAction = #selector(openSelected)
        table.target = self
        scroll.documentView = table

        status = NSTextField(labelWithString: "no connection selected")
        status.font = .systemFont(ofSize: 11)
        status.textColor = .secondaryLabelColor
        status.frame = NSRect(x: 12, y: 12, width: 596, height: 18)
        status.autoresizingMask = [.width, .maxYMargin]

        for v in [profilePopUp, addButton, refreshButton, newButton, scroll, status] as [NSView] {
            content.addSubview(v)
        }
        window?.contentView = content
    }

    private func refreshProfiles() {
        profilePopUp.removeAllItems()
        for p in profiles { profilePopUp.addItem(withTitle: "\(p.name) — \(p.baseURL)") }
        if !profiles.isEmpty { profilePopUp.selectItem(at: 0); refreshSessions() }
    }

    private func client() -> ApiClient? {
        guard let profile = selectedProfile,
              let credential = Keychain.credential(for: profile.name) else { return nil }
        return ApiClient(profile: profile, credential: credential)
    }

    @objc private func profileChanged() { refreshSessions() }

    @objc private func addConnection() {
        let alert = NSAlert()
        alert.messageText = "Add an openab-pty connection"
        alert.informativeText = "The admin credential is stored in your Keychain. It is what lets this app renew its own attach tokens instead of asking someone for a new one every time."
        alert.addButton(withTitle: "Save")
        alert.addButton(withTitle: "Cancel")

        let form = NSView(frame: NSRect(x: 0, y: 0, width: 380, height: 90))
        let nameField = NSTextField(frame: NSRect(x: 0, y: 62, width: 380, height: 24))
        nameField.placeholderString = "name (e.g. p1)"
        let urlField = NSTextField(frame: NSRect(x: 0, y: 32, width: 380, height: 24))
        urlField.placeholderString = "base URL (e.g. http://192.168.0.25:8090)"
        let credField = NSSecureTextField(frame: NSRect(x: 0, y: 2, width: 380, height: 24))
        credField.placeholderString = "admin credential"
        form.addSubview(nameField); form.addSubview(urlField); form.addSubview(credField)
        alert.accessoryView = form

        guard alert.runModal() == .alertFirstButtonReturn else { return }
        let name = nameField.stringValue.trimmingCharacters(in: .whitespaces)
        let url = urlField.stringValue.trimmingCharacters(in: .whitespaces)
        let cred = credField.stringValue.trimmingCharacters(in: .whitespaces)
        guard !name.isEmpty, !url.isEmpty, !cred.isEmpty else {
            status.stringValue = "all three fields are required"
            return
        }
        do {
            try Keychain.save(credential: cred, for: name)
        } catch {
            status.stringValue = error.localizedDescription
            return
        }
        profiles.removeAll { $0.name == name }
        profiles.append(Profile(baseURL: url, name: name))
        ProfileStore.save(profiles)
        refreshProfiles()
    }

    @objc private func refreshSessions() {
        guard let api = client(), let profile = selectedProfile else {
            status.stringValue = "no connection selected (or its credential is missing from the Keychain)"
            sessions = []; table.reloadData(); return
        }
        status.stringValue = "querying \(profile.baseURL)…"
        Task { @MainActor in
            do {
                let listing = try await api.list()
                self.sessions = listing.sessions
                self.table.reloadData()
                // Report best-effort semantics rather than implying a guarantee
                // the runtime explicitly does not make.
                var parts = ["\(listing.sessions.count) session(s)", listing.killDomain.tier]
                if listing.killDomain.teardownBestEffort { parts.append("teardown best-effort") }
                if listing.killDomain.leakedProcesses > 0 {
                    parts.append("⚠︎ \(listing.killDomain.leakedProcesses) leaked process(es)")
                }
                if listing.draining { parts.append("runtime is draining") }
                self.status.stringValue = parts.joined(separator: " · ")
            } catch {
                self.sessions = []; self.table.reloadData()
                self.status.stringValue = error.localizedDescription
            }
        }
    }

    @objc private func newSession() {
        guard let api = client() else { status.stringValue = "no connection selected"; return }
        let alert = NSAlert()
        alert.messageText = "New session"
        alert.informativeText = "Names must match [a-z0-9-] and be at most 32 characters."
        let field = NSTextField(frame: NSRect(x: 0, y: 0, width: 260, height: 24))
        field.stringValue = "mac"
        alert.accessoryView = field
        alert.addButton(withTitle: "Create")
        alert.addButton(withTitle: "Cancel")
        guard alert.runModal() == .alertFirstButtonReturn else { return }

        let name = field.stringValue.trimmingCharacters(in: .whitespaces)
        // Validate before the round trip: the runtime's own rule, applied here so
        // the user sees the problem immediately.
        guard !name.isEmpty, name.count <= 32,
              name.allSatisfy({ $0.isLowercase && $0.isLetter || $0.isNumber || $0 == "-" }) else {
            status.stringValue = "invalid name: expected [a-z0-9-]{1,32}"
            return
        }
        Task { @MainActor in
            do {
                let grant = try await api.create(name: name)
                self.openTerminal(name: name, token: grant.token)
                self.refreshSessions()
            } catch {
                self.status.stringValue = error.localizedDescription
            }
        }
    }

    @objc private func openSelected() {
        let row = table.selectedRow
        guard row >= 0, row < sessions.count, let api = client() else { return }
        let name = sessions[row].name
        Task { @MainActor in
            do {
                // Mint a token for this attach rather than reusing one we may not
                // have: renew keeps the shell and its scrollback.
                let grant = try await api.renew(name: name)
                self.openTerminal(name: name, token: grant.token)
            } catch {
                self.status.stringValue = error.localizedDescription
            }
        }
    }

    private func openTerminal(name: String, token: String) {
        guard let profile = selectedProfile, let api = client() else { return }
        let controller = TerminalWindowController(profile: profile, api: api,
                                                  sessionName: name, token: token)
        terminals.append(controller)
        controller.start()
    }

    // MARK: table

    func numberOfRows(in tableView: NSTableView) -> Int { sessions.count }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        let s = sessions[row]
        let text: String
        switch tableColumn?.identifier.rawValue {
        case "name": text = s.name
        case "state": text = s.alive ? (s.attached ? "attached" : "detached (alive)") : "dead"
        case "tier": text = s.teardownBestEffort ? "\(s.tier) (best-effort)" : s.tier
        case "bytes": text = "\(s.bytesWritten)"
        default: text = ""
        }
        let field = NSTextField(labelWithString: text)
        field.font = .systemFont(ofSize: 12)
        return field
    }
}
