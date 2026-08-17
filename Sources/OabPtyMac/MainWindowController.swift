import AppKit

/// One window: agent connections on the left, the active terminal on the right.
final class MainWindowController: NSWindowController, SidebarDelegate {
    private let split = NSSplitViewController()
    private let sidebar = SidebarViewController()
    private let detail = NSViewController()
    private var currentPane: TerminalPaneController?
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
        status.stringValue = "attaching to \(name)…"
        Task { @MainActor in
            do {
                // Renew rather than reuse: the shell and its scrollback survive,
                // and we get a token whose lifetime we know.
                let grant = try await api.renew(name: name)
                self.showPane(profile: profile, api: api, name: name, token: grant.token)
            } catch {
                self.status.stringValue = error.localizedDescription
            }
        }
    }

    private func showPane(profile: Profile, api: ApiClient, name: String, token: String) {
        currentPane?.stop()
        currentPane?.view.removeFromSuperview()
        currentPane?.removeFromParent()
        placeholder.isHidden = true

        let pane = TerminalPaneController(profile: profile, api: api, sessionName: name, token: token)
        detail.addChild(pane)
        pane.view.translatesAutoresizingMaskIntoConstraints = false
        detail.view.addSubview(pane.view)
        NSLayoutConstraint.activate([
            pane.view.topAnchor.constraint(equalTo: detail.view.topAnchor),
            pane.view.leadingAnchor.constraint(equalTo: detail.view.leadingAnchor),
            pane.view.trailingAnchor.constraint(equalTo: detail.view.trailingAnchor),
            pane.view.bottomAnchor.constraint(equalTo: detail.view.bottomAnchor)
        ])
        currentPane = pane
        pane.viewDidAppear()
        status.stringValue = "attached to \(name) on \(profile.name)"
    }
}
