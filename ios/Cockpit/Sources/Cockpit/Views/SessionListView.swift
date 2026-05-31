// D2 — Session list screen
import SwiftUI

public struct SessionListView: View {
    @EnvironmentObject var store: SessionStore

    public init() {}

    public var body: some View {
        Group {
            if store.sessions.isEmpty {
                ContentUnavailableView(
                    "No sessions",
                    systemImage: "terminal",
                    description: Text("Launch a new session to get started.")
                )
            } else {
                List(store.sessions) { session in
                    NavigationLink {
                        SessionDetailView(session: session)
                    } label: {
                        SessionRowView(session: session)
                    }
                }
            }
        }
    }
}

struct SessionRowView: View {
    let session: Session

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text(session.title ?? session.id.prefix(8).description)
                    .font(.headline)
                Spacer()
                StatusBadge(status: session.status)
            }
            HStack {
                Text(session.agentType)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Text("·")
                    .foregroundStyle(.secondary)
                Text(session.repoPath)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
        }
        .padding(.vertical, 4)
    }
}

struct StatusBadge: View {
    let status: SessionStatus

    var body: some View {
        Text(statusLabel)
            .font(.caption2.weight(.semibold))
            .foregroundStyle(statusColor)
    }

    private var statusLabel: String {
        switch status {
        case .active:         return "active"
        case .awaitingInput:  return "waiting"
        case .paused:         return "paused"
        case .stale:          return "stale"
        case .completed:      return "done"
        case .failed:         return "failed"
        case .killed:         return "killed"
        case .disconnected:   return "disconnected"
        case .unknown(let s): return s
        }
    }

    private var statusColor: Color {
        switch status {
        case .active:         return .green
        case .awaitingInput:  return .orange
        case .paused:         return .yellow
        case .stale:          return .gray
        case .completed:      return .blue
        case .failed:         return .red
        case .killed:         return .red
        case .disconnected:   return .gray
        case .unknown:        return .secondary
        }
    }
}
