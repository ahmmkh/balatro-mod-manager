import { fireEvent, render, screen, waitFor } from "@testing-library/svelte";
import { beforeEach, describe, expect, it, vi } from "vitest";
import ReportIssue from "./ReportIssue.svelte";

const state = vi.hoisted(() => {
  let value = { visible: true };
  const subs = new Set<(v: { visible: boolean }) => void>();
  return {
    subscribe: (run: (v: { visible: boolean }) => void) => {
      run(value);
      subs.add(run);
      return () => subs.delete(run);
    },
    set: (next: { visible: boolean }) => {
      value = next;
      subs.forEach((run) => run(value));
    },
  };
});
const invokeMock = vi.hoisted(() => vi.fn());
const addMessageMock = vi.hoisted(() => vi.fn());

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}));
vi.mock("$lib/stores", () => ({
  addMessage: (...args: unknown[]) => addMessageMock(...args),
}));
vi.mock("../stores/modStore", () => ({
  reportIssueStore: { subscribe: state.subscribe, set: state.set },
}));
// happy-dom has no Web Animations API; skip transitions.
vi.mock("svelte/transition", () => ({ fade: () => ({}), scale: () => ({}) }));
vi.mock("./DiscordFab.svelte", () => ({ default: () => ({}) }));

const preview = {
  os: "macos",
  arch: "aarch64",
  cpu: "cpu",
  gpu: "gpu",
  ram: "16 GB",
  log_filename: "bmm.log",
  log: "opened ~/Library/x for /Users/<user>/y",
  log_bytes: 38,
};

describe("ReportIssue", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    state.set({ visible: true });
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "preview_report") return preview;
      if (cmd === "get_app_version") return "0.4.1";
      return undefined;
    });
  });

  it("loads the preview when opened and shows the exact log in a scrollable region", async () => {
    render(ReportIssue);
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith("preview_report"));
    const log = await screen.findByTestId("report-log");
    expect((log as HTMLTextAreaElement).value).toBe(preview.log);
    expect(log.hasAttribute("readonly")).toBe(true);
  });

  it("keeps submit disabled until the consent box is ticked", async () => {
    render(ReportIssue);
    await screen.findByTestId("report-log");
    const submit = screen.getByRole("button", { name: "Submit" });
    expect((submit as HTMLButtonElement).disabled).toBe(true);
    await fireEvent.click(
      screen.getByLabelText("I understand this report includes the log shown below"),
    );
    expect((submit as HTMLButtonElement).disabled).toBe(false);
  });

  it("submits once with title, description and version only", async () => {
    render(ReportIssue);
    await screen.findByTestId("report-log");
    await fireEvent.input(screen.getByLabelText("Title"), { target: { value: "Crash" } });
    await fireEvent.input(screen.getByLabelText("Description"), { target: { value: "It broke" } });
    await fireEvent.click(
      screen.getByLabelText("I understand this report includes the log shown below"),
    );
    await fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    await waitFor(() =>
      expect(invokeMock.mock.calls.filter((c) => c[0] === "submit_report")).toHaveLength(1),
    );
    const call = invokeMock.mock.calls.find((c) => c[0] === "submit_report")!;
    expect(call[1]).toEqual({ title: "Crash", description: "It broke", mmVersion: "0.4.1" });
  });
});
