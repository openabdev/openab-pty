import AppKit

/// One window: agent connections on the left, the active terminal on the right.
final class MainWindowController: NSWindowController, SidebarDelegate {
    private let split = NSSplitViewController()
    private let sidebar = SidebarViewController()
    private let detail = NSViewController()
    private var currentPane: TerminalPaneController?
    /// Live panes by session name. Double-clicking a session that is already open
    /// must not mint a new token: renew bumps the generation, which evicts the
    /// connection currently holding that session -- so a second double-click was
    /// disconnecting the user from the very terminal they were pointing at.
    private var panes: [String: TerminalPaneController] = [:]
    private var refreshTimer: Timer?
    private var placeholder: NSTextField!
    private var status: NSTextField!

    init() {
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 1060, height: 620),
                              styleMask: [.titled, .closable, .resizable, .miniaturizable],
                              backing: .buffered, defer: false)
        window.title = "openab-pty"
        window.minSize = NSSize(width: 760, height: 420)
        super.init(window: window)

        detail.view = NSView(frame: NSRect(x: 0, y: 0, width: 780, height: 620))
        placeholder = NSTextField(labelWithString:
            "Add an agent connection on the left, then double-click a session to attach.")
        placeholder.textColor = .secondaryLabelColor
        placeholder.translatesAutoresizingMaskIntoConstraints = false
        detail.view.addSubview(placeholder)
        NSLayoutConstraint.activate([
            placeholder.centerXAnchor.constraint(equalTo: detail.view.centerXAnchor),
            placeholder.centerYAnchor.constraint(equalTo: detail.view.centerYAnchor)
        ])

        sidebar.delegate = self

        let sidebarItem = NSSplitViewItem(sidebarWithViewController: sidebar)
        sidebarItem.minimumThickness = 220
        sidebarItem.maximumThickness = 360
        sidebarItem.canCollapse = true
        let detailItem = NSSplitViewItem(viewController: detail)
        detailItem.minimumThickness = 480
        split.addSplitViewItem(sidebarItem)
        split.addSplitViewItem(detailItem)

        // Status line spans the bottom so a connection error is visible without
        // opening anything.
        status = NSTextField(labelWithString: "no connection yet")
        status.font = .systemFont(ofSize: 11)
        status.textColor = .secondaryLabelColor
        status.translatesAutoresizingMaskIntoConstraints = false

        let root = NSView(frame: window.contentView!.bounds)
        split.view.translatesAutoresizingMaskIntoConstraints = false
        root.addSubview(split.view)
        root.addSubview(status)
        NSLayoutConstraint.activate([
            split.view.topAnchor.constraint(equalTo: root.topAnchor),
            split.view.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            split.view.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            split.view.bottomAnchor.constraint(equalTo: status.topAnchor, constant: -4),
            status.leadingAnchor.constraint(equalTo: root.leadingAnchor, constant: 12),
            status.trailingAnchor.constraint(equalTo: root.trailingAnchor, constant: -12),
            status.bottomAnchor.constraint(equalTo: root.bottomAnchor, constant: -8)
        ])
        window.contentView = root
        contentViewController = nil
        startPeriodicRefresh()
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    // MARK: SidebarDelegate

    func sidebar(_ sidebar: SidebarViewController, didReportStatus text: String) {
        status.stringValue = text
    }

    func sidebar(_ sidebar: SidebarViewController, didChoose session: SessionNode) {
        guard let api = sidebar.apiClient(for: session.connection) else {
            status.stringValue = "credential for \(session.connection.profile.name) is missing from the Keychain"
            return
        }
        let name = session.info.name
        let profile = session.connection.profile

        // Already open: show it. Renewing here would evict this very connection.
        if let existing = panes[name] {
            present(existing)
            status.stringValue = "showing \(name) on \(profile.name)"
            return
        }

        status.stringValue = "attaching to \(name)…"
        Task { @MainActor in
            do {
                // Renew rather than reuse a token we may not hold: the shell and
                // its scrollback survive, and the lifetime is then known.
                let grant = try await api.renew(name: name)
                self.showPane(profile: profile, api: api, name: name, token: grant.token)
                // The list is a snapshot; without this the row still reads
                // "detached" while the terminal beside it is plainly attached.
                sidebar.refresh()
            } catch {
                self.status.stringValue = error.localizedDescription
            }
        }
    }

    private func showPane(profile: Profile, api: ApiClient, name: String, token: String) {
        let pane = TerminalPaneController(profile: profile, api: api, sessionName: name, token: token)
        detail.addChild(pane)
        panes[name] = pane
        present(pane)
        pane.viewDidAppear()
        status.stringValue = "attached to \(name) on \(profile.name)"
    }

    /// Bring a pane to the front. Panes are kept alive rather than torn down, so
    /// switching between sessions does not drop their connections.
    private func present(_ pane: TerminalPaneController) {
        currentPane?.view.isHidden = true
        placeholder.isHidden = true
        if pane.view.superview == nil {
            pane.view.translatesAutoresizingMaskIntoConstraints = false
            detail.view.addSubview(pane.view)
            NSLayoutConstraint.activate([
                pane.view.topAnchor.constraint(equalTo: detail.view.topAnchor),
                pane.view.leadingAnchor.constraint(equalTo: detail.view.leadingAnchor),
                pane.view.trailingAnchor.constraint(equalTo: detail.view.trailingAnchor),
                pane.view.bottomAnchor.constraint(equalTo: detail.view.bottomAnchor)
            ])
        }
        pane.view.isHidden = false
        currentPane = pane
        window?.makeFirstResponder(pane.view)
    }

    /// Keep the list honest without the user pressing refresh: attached/detached
    /// is exactly the kind of state that changes underneath a snapshot.
    private func startPeriodicRefresh() {
        refreshTimer?.invalidate()
        let timer = Timer(timeInterval: 5, repeats: true) { [weak self] _ in
            guard let self, self.window?.isVisible == true else { return }
            self.sidebar.refresh()
        }
        RunLoop.main.add(timer, forMode: .common)
        refreshTimer = timer
    }
}
