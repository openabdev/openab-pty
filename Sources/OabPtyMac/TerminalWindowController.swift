import AppKit
import SwiftTerm

/// A terminal window bound to one openab-pty session.
///
/// Recovery is automatic and silent where it can be: on a rejected attach the
/// client asks the admin API whether the session still exists, renews the token
/// if it does, and only involves the user when the session is genuinely gone.
/// That is the whole point of holding the credential locally.
final class TerminalWindowController: NSWindowController, TerminalViewDelegate {
    private let profile: Profile
    private let api: ApiClient
    private let sessionName: String
    private var token: String
    private var terminal: TerminalView!
    private var connection: AttachConnection?
    private var banner: NSTextField!
    private var reconnecting = false

    init(profile: Profile, api: ApiClient, sessionName: String, token: String) {
        self.profile = profile
        self.api = api
        self.sessionName = sessionName
        self.token = token

        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 900, height: 560),
            styleMask: [.titled, .closable, .resizable, .miniaturizable],
            backing: .buffered, defer: false)
        window.title = "\(sessionName) — \(profile.name)"
        super.init(window: window)

        let container = NSView(frame: window.contentView!.bounds)
        container.autoresizingMask = [.width, .height]

        banner = NSTextField(labelWithString: "connecting…")
        banner.font = .systemFont(ofSize: 11)
        banner.textColor = .secondaryLabelColor
        banner.frame = NSRect(x: 8, y: container.bounds.height - 20, width: container.bounds.width - 16, height: 16)
        banner.autoresizingMask = [.width, .minYMargin]

        terminal = TerminalView(frame: NSRect(x: 0, y: 0,
                                              width: container.bounds.width,
                                              height: container.bounds.height - 24))
        terminal.autoresizingMask = [.width, .height]
        terminal.terminalDelegate = self

        container.addSubview(terminal)
        container.addSubview(banner)
        window.contentView = container
        window.makeFirstResponder(terminal)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    func start() {
        connect(since: nil)
        showWindow(nil)
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
                var parts = ["attached at offset \(offset)"]
                if ephemeral { parts.append("workspace is ephemeral — \(externalise)") }
                if bestEffort { parts.append("teardown is best-effort") }
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

        // Never opened means the handshake itself was refused, which the runtime
        // reports as 401 for BOTH an expired token and a missing session. Resolve
        // it rather than telling the user something that might be false.
        if !everOpened {
            guard !reconnecting else { return }
            reconnecting = true
            setBanner("connection refused — checking whether the session still exists…", warning: true)
            Task { @MainActor in
                defer { self.reconnecting = false }
                do {
                    switch try await self.api.resolveRejectedAttach(name: self.sessionName) {
                    case .renewed(let grant):
                        self.token = grant.token
                        self.setBanner("token had expired — renewed, reconnecting")
                        self.connect(since: offset)
                    case .sessionGone:
                        self.offerRestart()
                    }
                } catch {
                    self.setBanner("could not recover: \(error.localizedDescription)", warning: true)
                }
            }
            return
        }

        let (title, detail) = CloseReason.describe(code)
        setBanner("\(title) — \(detail)", warning: true)

        // A rotated token is not an error: the shell is still there.
        if code == 4003 {
            Task { @MainActor in
                if let grant = try? await self.api.renew(name: self.sessionName) {
                    self.token = grant.token
                    self.connect(since: offset)
                }
            }
        }
    }

    private func offerRestart() {
        let alert = NSAlert()
        alert.messageText = "The session “\(sessionName)” is gone"
        alert.informativeText = "Its shell exited, or it passed a TTL. Start a fresh one under the same name?"
        alert.addButton(withTitle: "Restart")
        alert.addButton(withTitle: "Close")
        guard alert.runModal() == .alertFirstButtonReturn else {
            window?.close()
            return
        }
        Task { @MainActor in
            do {
                let grant = try await self.api.create(name: self.sessionName)
                self.token = grant.token
                self.terminal.feed(text: "\r\n[new session]\r\n")
                self.connect(since: nil)
            } catch {
                self.setBanner("restart failed: \(error.localizedDescription)", warning: true)
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
        window?.title = title.isEmpty ? "\(sessionName) — \(profile.name)" : title
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
