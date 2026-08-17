import Foundation

/// Server → client control frames, and the close reasons the runtime defines.
enum AttachEvent {
    case notice(streamOffset: Int, ephemeralWorkspace: Bool, externaliseWith: String, teardownBestEffort: Bool)
    case bytes([UInt8])
    case gap(droppedBytes: Int)
    case ttlWarning
    /// `code` is the runtime's own close code, or 1006-style transport failure.
    case closed(code: Int, reason: String, everOpened: Bool)
    case failed(String)
}

/// Contract §5. Naming these is the difference between a user knowing what
/// happened and seeing "disconnected".
enum CloseReason {
    static func describe(_ code: Int) -> (title: String, detail: String) {
        switch code {
        case 4001: return ("Session expired", "It passed its idle or absolute lifetime. Create a new one.")
        case 4002: return ("Taken over", "Another device attached to this same session and now holds the keyboard. Sessions are single-attach by design.")
        case 4003: return ("Token rotated", "An admin renewed this session. The shell is still alive; reconnecting with the new token.")
        case 4004: return ("Shell exited", "The command in the session ended by itself. Restart the session for a fresh shell.")
        case 4005: return ("Disconnected: too slow", "This client could not keep up with the output stream.")
        case 4006: return ("Server replaced", "The runtime restarted. The workspace is ephemeral, so anything not pushed is gone.")
        case 4007: return ("At capacity", "The runtime hit a session or tracking limit.")
        case 4008: return ("Killed by operator", "Someone terminated this session through the admin plane.")
        case 4009: return ("Internal error", "The runtime hit an internal fault. Retrying will not help.")
        default:   return ("Disconnected", "Closed with code \(code).")
        }
    }
}

/// One attach connection.
///
/// Two things here exist purely because of measurements taken during dogfooding,
/// not because the protocol demands them:
///
/// - **Keepalive.** Echo latency was bimodal (a few samples at 4 ms among many at
///   ~80 ms) with 0% packet loss and 41.5 ms of jitter, which is WiFi power
///   saving rather than distance. The protocol's own ping is 15–30 s, three
///   orders of magnitude too slow to hold a radio awake.
/// - **Offset tracking.** The runtime supports `?since=` replay; the first client
///   ignored it and restarted from scratch on every reconnect.
final class AttachConnection: NSObject, URLSessionWebSocketDelegate {
    private var task: URLSessionWebSocketTask?
    private var session: URLSession!
    private var keepalive: Timer?
    private var everOpened = false
    private(set) var streamOffset: Int?

    private let onEvent: (AttachEvent) -> Void

    /// ~50 ms: fast enough to keep a WiFi radio out of power save, which is what
    /// the jitter measurements pointed at.
    private let keepaliveInterval: TimeInterval = 0.05

    init(onEvent: @escaping (AttachEvent) -> Void) {
        self.onEvent = onEvent
        super.init()
        self.session = URLSession(configuration: .ephemeral, delegate: self, delegateQueue: .main)
    }

    func connect(profile: Profile, session name: String, token: String, since: Int?) {
        guard let url = profile.webSocketURL(session: name, since: since) else {
            onEvent(.failed("Malformed URL for session \(name)"))
            return
        }
        var req = URLRequest(url: url)
        // The reason a native client is simpler than a browser one: a header.
        req.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        streamOffset = since
        everOpened = false
        let t = session.webSocketTask(with: req)
        task = t
        t.resume()
        receive()
    }

    func disconnect() {
        keepalive?.invalidate()
        keepalive = nil
        task?.cancel(with: .goingAway, reason: nil)
        task = nil
    }

    func send(bytes: ArraySlice<UInt8>) {
        task?.send(.data(Data(bytes))) { _ in }
    }

    func resize(cols: Int, rows: Int) {
        let frame = #"{"v":1,"type":"resize","cols":\#(cols),"rows":\#(rows)}"#
        task?.send(.string(frame)) { _ in }
    }

    private func startKeepalive() {
        keepalive?.invalidate()
        let timer = Timer(timeInterval: keepaliveInterval, repeats: true) { [weak self] _ in
            self?.task?.sendPing { _ in }
        }
        RunLoop.main.add(timer, forMode: .common)
        keepalive = timer
    }

    private func receive() {
        task?.receive { [weak self] result in
            guard let self else { return }
            switch result {
            case .failure(let error):
                self.keepalive?.invalidate()
                // A rejected upgrade surfaces here with no close frame — the
                // transport-level equivalent of the browser's 1006. The caller
                // must resolve it through the admin API rather than guess.
                self.onEvent(.closed(code: 1006, reason: error.localizedDescription,
                                     everOpened: self.everOpened))
            case .success(let message):
                switch message {
                case .data(let data):
                    let bytes = [UInt8](data)
                    self.streamOffset = (self.streamOffset ?? 0) + bytes.count
                    self.onEvent(.bytes(bytes))
                case .string(let text):
                    self.handleControl(text)
                @unknown default:
                    break
                }
                self.receive()
            }
        }
    }

    private func handleControl(_ text: String) {
        guard let data = text.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let type = obj["type"] as? String else { return }
        switch type {
        case "attach-notice":
            let offset = obj["stream_offset"] as? Int ?? 0
            streamOffset = offset
            onEvent(.notice(streamOffset: offset,
                            ephemeralWorkspace: obj["ephemeral_workspace"] as? Bool ?? false,
                            externaliseWith: obj["externalise_with"] as? String ?? "",
                            teardownBestEffort: obj["teardown_best_effort"] as? Bool ?? false))
        case "gap":
            onEvent(.gap(droppedBytes: obj["dropped_bytes"] as? Int ?? 0))
        case "ttl-warning":
            onEvent(.ttlWarning)
        default:
            break
        }
    }

    // MARK: URLSessionWebSocketDelegate

    func urlSession(_ session: URLSession, webSocketTask: URLSessionWebSocketTask,
                    didOpenWithProtocol proto: String?) {
        everOpened = true
        startKeepalive()
    }

    func urlSession(_ session: URLSession, webSocketTask: URLSessionWebSocketTask,
                    didCloseWith closeCode: URLSessionWebSocketTask.CloseCode,
                    reason: Data?) {
        keepalive?.invalidate()
        let text = reason.flatMap { String(data: $0, encoding: .utf8) } ?? ""
        onEvent(.closed(code: closeCode.rawValue, reason: text, everOpened: everOpened))
    }
}
