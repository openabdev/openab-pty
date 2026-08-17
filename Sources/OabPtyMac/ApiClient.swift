import Foundation

struct SessionInfo: Decodable {
    let name: String
    let generation: Int
    let alive: Bool
    let attached: Bool
    let bytesWritten: Int
    let tier: String
    let teardownBestEffort: Bool

    enum CodingKeys: String, CodingKey {
        case name, generation, alive, attached, tier
        case bytesWritten = "bytes_written"
        case teardownBestEffort = "teardown_best_effort"
    }
}

struct KillDomain: Decodable {
    let tier: String
    let teardownBestEffort: Bool
    let leakedProcesses: Int
    let trackedProcesses: Int

    enum CodingKeys: String, CodingKey {
        case tier
        case teardownBestEffort = "teardown_best_effort"
        case leakedProcesses = "leaked_processes"
        case trackedProcesses = "tracked_processes"
    }
}

struct SessionList: Decodable {
    let sessions: [SessionInfo]
    let draining: Bool
    let killDomain: KillDomain

    enum CodingKeys: String, CodingKey {
        case sessions, draining
        case killDomain = "kill_domain"
    }
}

struct AttachGrant: Decodable {
    let session: String
    let generation: Int
    let token: String
    let tokenExpiresInSecs: Int

    enum CodingKeys: String, CodingKey {
        case session, generation, token
        case tokenExpiresInSecs = "token_expires_in_secs"
    }
}

enum ApiError: LocalizedError {
    case unauthorized
    case server(String)
    case transport(String)
    case badResponse

    var errorDescription: String? {
        switch self {
        case .unauthorized: return "Rejected: the admin credential was not accepted."
        case .server(let m): return m
        case .transport(let m): return m
        case .badResponse: return "The runtime returned something this client could not parse."
        }
    }
}

/// Client for the five admin endpoints plus the attach surface.
///
/// Deliberately uses `Authorization: Bearer`. The
/// `Sec-WebSocket-Protocol: openab.bearer.<token>` form exists only because
/// browsers cannot set headers on an upgrade; a native client should not inherit
/// that workaround.
final class ApiClient {
    private let profile: Profile
    private let credential: String
    private let session: URLSession

    init(profile: Profile, credential: String) {
        self.profile = profile
        self.credential = credential
        let cfg = URLSessionConfiguration.ephemeral
        cfg.timeoutIntervalForRequest = 15
        self.session = URLSession(configuration: cfg)
    }

    private func request(_ method: String, _ path: String, body: Data? = nil) throws -> URLRequest {
        guard let base = profile.httpBase, let url = URL(string: path, relativeTo: base) else {
            throw ApiError.transport("Malformed base URL: \(profile.baseURL)")
        }
        var req = URLRequest(url: url)
        req.httpMethod = method
        req.setValue("Bearer \(credential)", forHTTPHeaderField: "Authorization")
        if let body {
            req.httpBody = body
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }
        return req
    }

    private func send<T: Decodable>(_ req: URLRequest, as _: T.Type) async throws -> T {
        let (data, response): (Data, URLResponse)
        do {
            (data, response) = try await session.data(for: req)
        } catch {
            throw ApiError.transport(error.localizedDescription)
        }
        guard let http = response as? HTTPURLResponse else { throw ApiError.badResponse }
        if http.statusCode == 401 { throw ApiError.unauthorized }
        if http.statusCode >= 400 {
            // The runtime reports failures as {"error": "..."}; surface its own
            // words rather than a status code, since they are actionable
            // ("no such session: x", "session capacity exceeded (limit 3)").
            if let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
               let message = obj["error"] as? String {
                throw ApiError.server(message)
            }
            throw ApiError.server("HTTP \(http.statusCode)")
        }
        guard let decoded = try? JSONDecoder().decode(T.self, from: data) else {
            throw ApiError.badResponse
        }
        return decoded
    }

    func list() async throws -> SessionList {
        try await send(request("GET", "/admin/sessions"), as: SessionList.self)
    }

    func create(name: String) async throws -> AttachGrant {
        let body = try JSONSerialization.data(withJSONObject: ["name": name])
        return try await send(request("POST", "/admin/sessions", body: body), as: AttachGrant.self)
    }

    func renew(name: String) async throws -> AttachGrant {
        try await send(request("POST", "/admin/sessions/\(name)/renew"), as: AttachGrant.self)
    }

    func restart(name: String) async throws -> AttachGrant {
        try await send(request("POST", "/admin/sessions/\(name)/restart"), as: AttachGrant.self)
    }

    func kill(name: String) async throws {
        var req = try request("DELETE", "/admin/sessions/\(name)")
        req.httpMethod = "DELETE"
        _ = try? await session.data(for: req)
    }

    /// Outcome of resolving a rejected attach.
    enum Recovery {
        /// The session exists; this token supersedes the stale one.
        case renewed(AttachGrant)
        /// The session is gone — the shell exited or a TTL elapsed.
        case sessionGone
    }

    /// Contract §6. The runtime answers `401` for an expired token *and* for a
    /// session that does not exist, on purpose, so names cannot be enumerated.
    /// Those need opposite responses, and guessing gets it wrong: during
    /// dogfooding a user was told "your token expired" when the audit log showed
    /// the shell had exited and the session needed recreating. The credential is
    /// already here, so resolve it properly instead of guessing.
    func resolveRejectedAttach(name: String) async throws -> Recovery {
        let listing = try await list()
        guard listing.sessions.contains(where: { $0.name == name }) else {
            return .sessionGone
        }
        return .renewed(try await renew(name: name))
    }
}
