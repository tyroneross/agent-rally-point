// CV6-A — QR scanner sheet (UIViewControllerRepresentable wrapping AVCaptureSession).
// TAG:UNTESTED — camera capture requires a physical device; the simulator has no camera.
// The view compiles and presents on the simulator, falling back to a manual paste affordance.

import SwiftUI
import AVFoundation

// MARK: - Sendable wrapper for AVCaptureSession
// AVCaptureSession is not declared Sendable but is documented as safe to start/stop
// from a background thread.  We own it exclusively here, so @unchecked Sendable is safe.

private struct SendableSession: @unchecked Sendable {
    let session: AVCaptureSession
}

// MARK: - UIKit camera controller

@MainActor
public final class QRScannerViewController: UIViewController {

    var onScan: ((String) -> Void)?

    private var captureSession: AVCaptureSession?
    private var previewLayer: AVCaptureVideoPreviewLayer?

    // Separate delegate object to avoid @MainActor protocol-crossing issues.
    private lazy var metaDelegate = QRMetadataDelegate { [weak self] string in
        MainActor.assumeIsolated {
            self?.captureSession?.stopRunning()
            self?.onScan?(string)
        }
    }

    override public func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .black
        checkPermissionAndSetup()
    }

    override public func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        if let session = captureSession {
            let box = SendableSession(session: session)
            Task.detached(priority: .userInitiated) { box.session.startRunning() }
        }
    }

    override public func viewWillDisappear(_ animated: Bool) {
        super.viewWillDisappear(animated)
        if let session = captureSession {
            let box = SendableSession(session: session)
            Task.detached(priority: .userInitiated) { box.session.stopRunning() }
        }
    }

    // MARK: - Permission + session setup

    private func checkPermissionAndSetup() {
        switch AVCaptureDevice.authorizationStatus(for: .video) {
        case .authorized:
            setupCaptureSession()
        case .notDetermined:
            Task { @MainActor [weak self] in
                let granted = await AVCaptureDevice.requestAccess(for: .video)
                if granted { self?.setupCaptureSession() }
                else       { self?.showNoCameraMessage("Camera access denied.") }
            }
        case .denied, .restricted:
            showNoCameraMessage("Camera access denied.\nEnable it in Settings → Privacy → Camera.")
        @unknown default:
            showNoCameraMessage("Camera unavailable.")
        }
    }

    private func setupCaptureSession() {
        let session = AVCaptureSession()

        guard let device = AVCaptureDevice.default(for: .video),
              let input  = try? AVCaptureDeviceInput(device: device),
              session.canAddInput(input)
        else {
            showNoCameraMessage("No camera detected.\nUse the paste field below.")
            return
        }

        session.addInput(input)

        let metaOutput = AVCaptureMetadataOutput()
        guard session.canAddOutput(metaOutput) else {
            showNoCameraMessage("Camera setup failed.")
            return
        }
        session.addOutput(metaOutput)
        metaOutput.setMetadataObjectsDelegate(metaDelegate, queue: .main)
        metaOutput.metadataObjectTypes = [.qr]

        let preview = AVCaptureVideoPreviewLayer(session: session)
        preview.frame = view.layer.bounds
        preview.videoGravity = .resizeAspectFill
        view.layer.addSublayer(preview)
        previewLayer = preview
        captureSession = session

        let box = SendableSession(session: session)
        Task.detached(priority: .userInitiated) { box.session.startRunning() }
    }

    private func showNoCameraMessage(_ message: String) {
        let label = UILabel()
        label.text = message
        label.numberOfLines = 0
        label.textAlignment = .center
        label.textColor = .secondaryLabel
        label.font = .preferredFont(forTextStyle: .body)
        label.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(label)
        NSLayoutConstraint.activate([
            label.centerXAnchor.constraint(equalTo: view.centerXAnchor),
            label.centerYAnchor.constraint(equalTo: view.centerYAnchor, constant: -40),
            label.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 24),
            label.trailingAnchor.constraint(equalTo: view.trailingAnchor, constant: -24),
        ])
    }
}

// MARK: - Metadata delegate

/// Thin AVCaptureMetadataOutputObjectsDelegate — forwards the first QR string via closure.
/// Kept as a separate NSObject to avoid @MainActor ↔ protocol-delegate crossing issues.
final class QRMetadataDelegate: NSObject, AVCaptureMetadataOutputObjectsDelegate, @unchecked Sendable {
    private let onScan: @MainActor (String) -> Void
    init(onScan: @escaping @MainActor (String) -> Void) { self.onScan = onScan }

    func metadataOutput(
        _ output: AVCaptureMetadataOutput,
        didOutput metadataObjects: [AVMetadataObject],
        from connection: AVCaptureConnection
    ) {
        guard let obj = metadataObjects.first as? AVMetadataMachineReadableCodeObject,
              let string = obj.stringValue
        else { return }
        // Delegate fires on .main queue (set above), so we can call @MainActor directly.
        MainActor.assumeIsolated { onScan(string) }
    }
}

// MARK: - SwiftUI wrapper

/// Camera QR scanner. `onScan` fires once with the raw QR string.
/// On the simulator (or after permission denial) a "no camera" message is shown.
public struct QRScannerView: UIViewControllerRepresentable {

    public let onScan: (String) -> Void

    public init(onScan: @escaping (String) -> Void) {
        self.onScan = onScan
    }

    public func makeUIViewController(context: Context) -> QRScannerViewController {
        let vc = QRScannerViewController()
        vc.onScan = onScan
        return vc
    }

    public func updateUIViewController(_ uiViewController: QRScannerViewController, context: Context) {}
}

// MARK: - Full pairing sheet with paste fallback

/// Full-screen pairing sheet: live QR scanner + manual paste fallback in one view.
/// Embed in `.sheet(isPresented:)`.
public struct PairingScannerSheet: View {

    public let onScan: (String) -> Void
    @Environment(\.dismiss) private var dismiss

    @State private var pasteText = ""
    @State private var showPaste = false

    public init(onScan: @escaping (String) -> Void) {
        self.onScan = onScan
    }

    public var body: some View {
        NavigationStack {
            ZStack(alignment: .bottom) {
                QRScannerView(onScan: onScan)
                    .ignoresSafeArea()

                VStack(spacing: 12) {
                    if showPaste {
                        TextEditor(text: $pasteText)
                            .frame(height: 80)
                            .padding(8)
                            .background(Color(.systemBackground).opacity(0.9))
                            .cornerRadius(8)
                            .overlay(
                                RoundedRectangle(cornerRadius: 8)
                                    .stroke(Color.secondary.opacity(0.4))
                            )
                        Button("Use pasted payload") {
                            let trimmed = pasteText.trimmingCharacters(in: .whitespacesAndNewlines)
                            guard !trimmed.isEmpty else { return }
                            onScan(trimmed)
                        }
                        .buttonStyle(.borderedProminent)
                    } else {
                        Button("Paste payload instead") {
                            showPaste = true
                        }
                        .font(.footnote)
                        .padding(.vertical, 8)
                    }
                }
                .padding()
                .background(.ultraThinMaterial)
            }
            .navigationTitle("Scan to Pair")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
            }
        }
    }
}
