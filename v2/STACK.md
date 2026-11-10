# Lazybox V2 implementation stack

Status: stack direction accepted in the design discussion on 2026-10-25. Library integration and performance remain subject to the M0 experiments. Accepting a stack does not approve any V1 implementation for reuse.

**2026-11-09 scope:** phone/iPad development is part of the current release
direction, not post-desktop expansion. The Hub is implemented in Obin platform
V2; this repository consumes its contract. Shared TypeScript behavior and
device-specific composition follow [Device experience](DEVICE_EXPERIENCE.md).

## Selected languages and frameworks

| Component | Choice | Responsibility |
| --- | --- | --- |
| Hub | External Obin platform V2; implementation decisions owned there | Shared authority, identity, work records, assignments, attention and persistence. |
| Runner | Go | Agent adapters, process supervision, worktrees, command capture, service routing, and local journals. |
| Shared client core | TypeScript | Typed service access, records, commands, event reduction, reconnection, drafts, and portable surface references. |
| Desktop and web interface | React with TypeScript | Reusable work, execution, decision, evidence, and review surfaces. |
| Desktop host | Tauri 2 with a small Rust integration layer | Native windows, displays, clipboard, notifications, and OS integration. |
| Desktop and web docking | Evaluate Dockview | Tabs, split panes, docking, and layout serialization; native window detachment requires its own integration. |
| iPhone / iPad | Expo with React Native and TypeScript | Touch navigation, adaptive tablet composition, notifications, review, capture and access to shared work. |

Go is selected for service development and maintenance. The current workload calls for correct concurrency, bounded I/O, reliable process handling, and durable state; no measured requirement establishes a need to implement these services in Rust. Validate the actual workload rather than assuming a language choice meets performance budgets.

Tauri's Rust core manages native integration; the main interface renders through the system webview. UI responsiveness still depends on rendering, subscriptions, virtualization, batching, and avoiding unnecessary work. The native host stays independent of Hub and Runner lifetimes.

## Shared client and platform boundaries

Share the protocol client, record types, commands, state transitions, event reducers, identity references, reconnection rules, and suitable interaction logic across desktop, web, and iPhone. Keep platform APIs behind explicit interfaces.

React DOM components serve desktop and web. React Native supplies the phone's interface. Share appropriate primitives and product logic where useful; a DOM-based console, graph, docking layout, or browser preview does not automatically render as a native phone component.

Desktop windows use a platform host API for create, focus, close, restore, and display placement. Web uses browser-supported tabs/windows; iPhone uses navigation routes and sheets. Product surfaces depend on these capabilities through the shared contract, rather than importing Tauri into every component.

## Surface model

A surface describes a view of existing work, such as a work graph, agent conversation, command console, preview, or review. A surface has its own identity, kind, target references, navigation, and view state. Several surfaces may target the same command or work item.

Opening, detaching, copying, or restoring a surface does not start another execution. Moving a surface preserves its viewing context; opening another surface creates independent viewing state. Destructive actions and process control are explicit product operations.

| State | Owner | Continuation behavior |
| --- | --- | --- |
| Work, assignments, messages, decisions, executions, artifacts | Hub under shared project authority | Updates reach authorized participants through shared subscriptions. |
| Personal working sets, drafts, read position | Individual user | Synced personal context can continue on another device; unsent draft availability is explicit. |
| Selection, navigation, focus, scroll, geometry | Particular surface and device | Restore useful views without redirecting other windows; adapt geometry to available displays. |

Each native window has its own React root and interaction state. The client runtime handles shared record updates without broadcasting one window's selected item as global navigation. Local transport sharing is an implementation optimization; Hub records remain authoritative across processes and devices.

## Windowing and layout validation

Dockview is the candidate for layout inside a window. Its browser popout capability does not prove Tauri native detachment, cross-window drag and drop, or restoration. Validate those behaviors through the native host interface before adopting the library.

The first desktop proof detaches a live command console, closes the originating window, reconnects the viewer, and restores the surface after changing the display configuration. Execution continues independently. Test keyboard focus, selection/copy, output scrolling, and explicit read-only versus interactive console behavior.

## UI development workflow

Develop surfaces in isolation using reproducible scenarios: empty, loading, running, awaiting input, failed, disconnected, stale, and receiving sustained output. Use the same surface components in scenario previews and the actual app.

Exercise shared workflows with browser automation, then verify actual native windows, focus, displays, clipboard, and restoration in the desktop test tier. Native checks and multi-person tests remain necessary even when the browser workflow passes.

Phone testing includes background/foreground transitions and connection recovery on actual devices. Sharing TypeScript code does not establish that a streaming transport behaves identically in browser, WKWebView, and React Native.

## Contracts and integration choices

The implemented Runner communication stack is Protobuf with generated Go and TypeScript clients, hosted by Codefly `go-grpc:0.1.69`. A single gRPC implementation serves native calls and Codefly's generated Connect/browser listener. Network resolution, health, generation, and lifecycle belong to Codefly. Go Hub/Runner communication can use gRPC; browser-based clients use unary operations and server streams. See [RPC and agent interfaces](INTERFACES.md) for the proposal and browser limitations.

Local database choice, schema migrations, authentication, artifact storage, and deployment remain explicit decisions in [Decisions](DECISIONS.md). They are not settled by the language/framework selection.

## References

- [Tauri process model](https://v2.tauri.app/concept/process-model/) explains the Rust core and system webview boundary.
- [Dockview](https://dockview.dev/docs/overview/introduction/) documents docking and browser popouts; its [license matrix](https://dockview.dev/docs/overview/licence/) distinguishes core and enterprise capabilities.
- [Expo web support](https://docs.expo.dev/workflow/web/) distinguishes native components and web-only DOM components.
- [Tauri WebDriver testing](https://v2.tauri.app/develop/tests/webdriver/) documents the native test integration.
