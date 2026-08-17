import AppKit
import SwiftTerm

/// The terminal for one session, as a pane rather than a window so it can live in
/// the split view's detail area.
///
/// Recovery is deliberately silent where it can be: on a refused attach it asks
/// the admin API whether the session still exists, renews the token if so, and
/// only involves the user when the session is genuinely gone. Holding the
/// credential locally is what makes that possible.
final class TerminalPaneController: NSViewController, TerminalViewDelegate {
    private let profile: Profile
    private let api: ApiClient
    let sessionName: String
    private var token: String

    private var terminal: TerminalView!
    private var banner: NSTextField!
    private var connection: AttachConnection?
    private var reconnecting = false

    init(profile: Profile, api: ApiClient, sessionName: String, token: String) {
        self.profile = profile
        self.api = api
        self.sessionName = sessionName
        self.token = token
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override func loadView() {
        let container = NSView(frame: NSRect(x: 0, y: 0, width: 800, height: 520))

        banner = NSTextField(labelWithString: "connecting…")
        banner.font = .systemFont(ofSize: 11)
        banner.textColor = .secondaryLabelColor
        banner.translatesAutoresizingMaskIntoConstraints = false

        terminal = TerminalView(frame: .zero)
        terminal.translatesAutoresizingMaskIntoConstraints = false
        terminal.terminalDelegate = self

        container.addSubview(banner)
        container.addSubview(terminal)
        NSLayoutConstraint.activate([
            banner.topAnchor.constraint(equalTo: container.topAnchor, constant: 6),
            banner.leadingAnchor.constraint(equalTo: container.leadingAnchor, constant: 10),
            banner.trailingAnchor.constraint(equalTo: container.trailingAnchor, constant: -10),
            terminal.topAnchor.constraint(equalTo: banner.bottomAnchor, constant: 6),
            terminal.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            terminal.trailingAnchor.constraint(equalTo: container.trailingAnchor),
            terminal.bottomAnchor.constraint(equalTo: container.bottomAnchor)
        ])
        view = container
    }

    override func viewDidAppear() {
        super.viewDidAppear()
        view.window?.makeFirstResponder(terminal)
        if connection == nil { connect(since: nil) }
    }

    func stop() {
        connection?.disconnect()
        connection = nil
    }

    private func setBanner(_ text: String, warning: Bool = false) {
        banner.stringValue = text
        banner.textColor = warning ? .systemOrange : .secondaryLabelColor
    }

    private func connect(since: Int?) {
        let conn = AttachConnection { [weak self] event in
            guard let self else { return }
            switch event {
            case .notice(let offset, let ephemeral, let externalise, let bestEffort):
                var parts = ["\(self.sessionName) · offset \(offset)"]
                if ephemeral { parts.append("workspace is ephemeral — \(externalise)") }
                if bestEffort { parts.append("teardown best-effort") }
                self.setBanner(parts.joined(separator: " · "))
            case .bytes(let bytes):
                self.terminal.feed(byteArray: bytes[...])
            case .gap(let dropped):
                self.terminal.feed(text: "\r\n[\(dropped) bytes dropped — redrawing]\r\n")
            case .ttlWarning:
                self.setBanner("this session is about to expire", warning: true)
            case .closed(let code, let reason, let everOpened):
                self.handleClose(code: code, reason: reason, everOpened: everOpened)
            case .failed(let message):
                self.setBanner(message, warning: true)
            }
        }
        connection = conn
        conn.connect(profile: profile, session: sessionName, token: token, since: since)
    }

    private func handleClose(code: Int, reason: String, everOpened: Bool) {
        let offset = connection?.streamOffset
        connection = nil

        // Never opened => the handshake was refused, and the runtime answers 401
        // for BOTH an expired token and a missing session. Resolve it instead of
        // telling the user something that may well be false.
        if !everOpened {
            guard !reconnecting else { return }
            reconnecting = true
            setBanner("refused — checking whether the session still exists…", warning: true)
            Task { @MainActor in
                defer { self.reconnecting = false }
                do {
                    switch try await self.api.resolveRejectedAttach(name: self.sessionName) {
                    case .renewed(let grant):
                        self.token = grant.token
                        self.setBanner("token had expired — renewed, reconnecting")
                        self.connect(since: offset)
                    case .sessionGone:
                        self.setBanner("the shell exited; this session is gone. Create it again from the sidebar.",
                                       warning: true)
                    }
                } catch {
                    self.setBanner("could not recover: \(error.localizedDescription)", warning: true)
                }
            }
            return
        }

        let (title, detail) = CloseReason.describe(code)
        setBanner("\(title) — \(detail)", warning: true)

        // A rotated token is not a failure: the shell survived it.
        if code == 4003 {
            Task { @MainActor in
                if let grant = try? await self.api.renew(name: self.sessionName) {
                    self.token = grant.token
                    self.connect(since: offset)
                }
            }
        }
    }

    // MARK: TerminalViewDelegate

    func send(source: TerminalView, data: ArraySlice<UInt8>) {
        connection?.send(bytes: data)
    }

    func sizeChanged(source: TerminalView, newCols: Int, newRows: Int) {
        connection?.resize(cols: newCols, rows: newRows)
    }

    func setTerminalTitle(source: TerminalView, title: String) {
        view.window?.title = title.isEmpty ? "openab-pty" : title
    }

    func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {}
    func scrolled(source: TerminalView, position: Double) {}
    func requestOpenLink(source: TerminalView, link: String, params: [String: String]) {
        if let url = URL(string: link) { NSWorkspace.shared.open(url) }
    }
    func bell(source: TerminalView) { NSSound.beep() }
    func clipboardCopy(source: TerminalView, content: Data) {
        if let text = String(data: content, encoding: .utf8) {
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(text, forType: .string)
        }
    }
    func iTermContent(source: TerminalView, content: ArraySlice<UInt8>) {}
    func rangeChanged(source: TerminalView, startY: Int, endY: Int) {}
}
