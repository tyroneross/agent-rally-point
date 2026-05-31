// D2 — Session detail + event timeline
import SwiftUI

public struct SessionDetailView: View {
    @Environment(\.dismiss) private var dismiss
    @StateObject private var vm: SessionDetailViewModel

    // Takes the REAL store (passed from the list) so the timeline reads live events —
    // not a throwaway SessionStore().
    public init(session: Session, store: SessionStore) {
        _vm = StateObject(wrappedValue: SessionDetailViewModel(session: session, store: store))
    }

    public var body: some View {
        NavigationStack {
            VStack(spacing: 0) {
                // Timeline
                ScrollViewReader { proxy in
                    ScrollView {
                        LazyVStack(alignment: .leading, spacing: 8) {
                            ForEach(vm.events) { event in
                                EventRowView(event: event)
                                    .id(event.id)
                            }
                        }
                        .padding()
                    }
                    .onChange(of: vm.events.count) { _, _ in
                        if let last = vm.events.last {
                            withAnimation { proxy.scrollTo(last.id, anchor: .bottom) }
                        }
                    }
                }

                Divider()

                // D3 — Composer
                ComposerView(vm: vm)
            }
            .navigationTitle(vm.session.title ?? "Session")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    Button("Done") { dismiss() }
                }
            }
        }
        .onAppear { vm.onAppear() }
        .onReceive(NotificationCenter.default.publisher(for: UIApplication.didBecomeActiveNotification)) { _ in
            vm.refresh()
        }
    }
}

// MARK: - Event row

struct EventRowView: View {
    let event: Event

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text(event.kind)
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(kindColor)
                Text("·")
                    .foregroundStyle(.tertiary)
                Text(event.sender)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Spacer()
                Text("#\(event.seq)")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }

            Text(event.content)
                .font(.body)
                .textSelection(.enabled)
        }
        .padding(10)
        .background(kindBackground)
        .clipShape(RoundedRectangle(cornerRadius: 8))
    }

    private var kindColor: Color {
        switch event.kind {
        case "message":       return .primary
        case "tool_call":     return .orange
        case "tool_result":   return .green
        case "diff":          return .blue
        case "error":         return .red
        case "approval_request": return .yellow
        default:              return .secondary
        }
    }

    private var kindBackground: Color {
        switch event.kind {
        case "tool_call":  return Color.orange.opacity(0.08)
        case "tool_result": return Color.green.opacity(0.08)
        case "diff":       return Color.blue.opacity(0.08)
        case "error":      return Color.red.opacity(0.08)
        default:           return Color(.secondarySystemBackground)
        }
    }
}
