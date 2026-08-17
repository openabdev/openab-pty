import Foundation

/// Human labels for sessions, stored locally.
///
/// Deliberately **not** a server-side rename. A session name is its identity: it
/// keys the runtime's session map, forms the attach path `/pty/{session}`, is
/// bound into every attach token, and appears in audit events. Renaming would
/// have to move that identity and invalidate outstanding tokens, and would break
/// anything still holding the old name — a lot of machinery for what is usually
/// just "I want to remember what this one is for".
///
/// So the identity stays fixed and the human gets a label. The real name is still
/// shown beside it, because a label that hides the identity makes the audit log
/// impossible to match up.
enum SessionLabels {
    private static let key = "sessionLabels"

    /// Keyed per connection, so the same session name on two hosts can differ.
    private static func id(profile: String, session: String) -> String {
        profile + "/" + session
    }

    private static func all() -> [String: String] {
        UserDefaults.standard.dictionary(forKey: key) as? [String: String] ?? [:]
    }

    static func label(profile: String, session: String) -> String? {
        let value = all()[id(profile: profile, session: session)]
        return (value?.isEmpty ?? true) ? nil : value
    }

    static func set(_ label: String?, profile: String, session: String) {
        var map = all()
        let k = id(profile: profile, session: session)
        if let label, !label.isEmpty { map[k] = label } else { map.removeValue(forKey: k) }
        UserDefaults.standard.set(map, forKey: key)
    }
}
