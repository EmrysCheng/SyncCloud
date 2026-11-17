// @vitest-environment jsdom
import React from "react";
import { afterEach, expect, test, vi } from "vitest";
import { cleanup, render, screen, fireEvent } from "@testing-library/react";
import Folders from "./Folders";
const mocks = vi.hoisted(() => ({ listFolders: vi.fn(), syncNow: vi.fn() }));
vi.mock("../api", () => ({ api: mocks }));
const folder = { id: "f1", name: "测试任务", enabled: true, localPath: "D:\\test", backend: {type:"aliyun"}, direction:"bidirectional", status: {state:"idle",lastReport:null,lastSyncAt:null} };
afterEach(() => {cleanup(); vi.resetAllMocks();});
test("failed loading is visible instead of an empty task list", async () => {
  mocks.listFolders.mockRejectedValue("IPC unavailable");
  render(<Folders events={[]} onGoto={() => {}} />);
  expect((await screen.findByRole("alert")).textContent).toContain("IPC unavailable");
  expect(screen.queryByText("还没有同步文件夹")).toBeNull();
});
test("manual sync reports rejected command", async () => {
  mocks.listFolders.mockResolvedValue([folder]); mocks.syncNow.mockRejectedValue("同步任务已退出");
  render(<Folders events={[]} onGoto={() => {}} />);
  fireEvent.click(await screen.findByTitle("强制拉取远端元数据执行完整三方对比与同步"));
  expect((await screen.findByRole("status")).textContent).toContain("同步任务已退出");
});
test("manual sync acknowledges a queued request", async () => {
  mocks.listFolders.mockResolvedValue([folder]); mocks.syncNow.mockResolvedValue({folderId:"f1", disposition:"queued"});
  render(<Folders events={[]} onGoto={() => {}} />);
  fireEvent.click(await screen.findByTitle("强制拉取远端元数据执行完整三方对比与同步"));
  expect((await screen.findByRole("status")).textContent).toContain("同步请求已受理");
});
