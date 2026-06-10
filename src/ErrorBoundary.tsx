import { Component, type ReactNode, type ErrorInfo } from "react";

interface Props { children: ReactNode; }
interface State { error: Error | null; }

export default class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    // Log to browser console (visible in Tauri devtools) and attempt to
    // surface in the Rust log via the plugin's JS bridge.
    const msg = `[ErrorBoundary] ${error.message}\n${error.stack ?? ""}\nComponent stack:${info.componentStack}`;
    console.error(msg);
    // tauri-plugin-log exposes window.__TAURI_INTERNALS__ in webview; fall
    // back silently if it's unavailable (e.g. plain browser dev).
    try {
      (window as any).__TAURI_INTERNALS__?.invoke?.("plugin:log|error", { message: msg });
    } catch {}
  }

  render() {
    if (this.state.error) {
      return (
        <div style={{
          padding: "32px", fontFamily: "monospace", color: "#ff6b6b",
          background: "#1a1a2e", height: "100vh", boxSizing: "border-box",
        }}>
          <h2 style={{ margin: "0 0 16px" }}>Render crash</h2>
          <pre style={{ whiteSpace: "pre-wrap", fontSize: "12px", opacity: 0.85 }}>
            {this.state.error.stack ?? this.state.error.message}
          </pre>
          <button
            onClick={() => this.setState({ error: null })}
            style={{ marginTop: "24px", padding: "8px 16px", cursor: "pointer" }}
          >
            Try to recover
          </button>
        </div>
      );
    }
    return this.props.children;
  }
}
