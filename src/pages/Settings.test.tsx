// @vitest-environment jsdom
import React from "react";
import {afterEach,beforeEach,expect,test,vi} from "vitest";
import {cleanup,render,screen,fireEvent,waitFor} from "@testing-library/react";
import Settings from "./Settings";
const mocks=vi.hoisted(()=>({getAppInfo:vi.fn(),getSettings:vi.fn(),saveSettings:vi.fn(),isAutostart:vi.fn(),isShellMenuEnabled:vi.fn(),listFolders:vi.fn(),listCloudAccounts:vi.fn(),listLogFiles:vi.fn(),getCurrentLogDir:vi.fn()}));
vi.mock("../api",()=>({api:mocks}));
vi.mock("@tauri-apps/plugin-dialog",()=>({open:vi.fn()}));
const settings={concurrency:3,maxRetries:0,fullScanIntervalMin:15,largeFileThreshold:0,language:"zh-CN",closeToTray:true,baiduAppKey:"",baiduSecretKey:"",aliyunAppId:"",aliyunAppSecret:"",logLevel:"info",logToFile:true,deletionGuard:true,deletionGuardThreshold:50,customLogDir:null,activityRetentionDays:7,maxUploadKbps:0,maxDownloadKbps:0};
beforeEach(()=>{
  mocks.getAppInfo.mockResolvedValue({deviceName:"fixture",dataDir:"D:\\test",defaultDataDir:"D:\\test"});mocks.getSettings.mockResolvedValue(settings);
  mocks.isAutostart.mockResolvedValue(false);mocks.isShellMenuEnabled.mockResolvedValue(false);
  for(const fn of [mocks.listFolders,mocks.listCloudAccounts,mocks.listLogFiles])fn.mockResolvedValue([]);
  mocks.getCurrentLogDir.mockResolvedValue("D:\\test");mocks.saveSettings.mockResolvedValue(undefined);
});
afterEach(()=>{cleanup();vi.resetAllMocks();});
test("load failure prevents saving defaults over existing settings",async()=>{
  mocks.getSettings.mockRejectedValue("database unavailable");render(<Settings/>);
  expect((await screen.findByRole("alert")).textContent).toContain("database unavailable");
  expect(mocks.saveSettings).not.toHaveBeenCalled();
});
test("save errors are visible and zero retries are preserved",async()=>{
  mocks.saveSettings.mockRejectedValue("disk full");render(<Settings/>);
  fireEvent.click(await screen.findByText("保存同步设置"));
  expect((await screen.findByRole("alert")).textContent).toContain("disk full");
  expect(mocks.saveSettings.mock.calls[0][1].maxRetries).toBe(0);
  expect(mocks.saveSettings.mock.calls[0][1].largeFileThreshold).toBe(0);
});
test("immediate saves are serialized",async()=>{
  let release!:()=>void;mocks.saveSettings.mockImplementationOnce(()=>new Promise<void>(resolve=>{release=resolve;}));
  render(<Settings/>);
  const checkbox=await screen.findByLabelText("关闭窗口时最小化到系统托盘（后台继续同步，右键托盘图标可退出）");
  fireEvent.click(checkbox);await waitFor(()=>expect(mocks.saveSettings).toHaveBeenCalledTimes(1));
  fireEvent.click(checkbox);expect(mocks.saveSettings).toHaveBeenCalledTimes(1);
  release();await waitFor(()=>expect(mocks.saveSettings).toHaveBeenCalledTimes(2));
  expect(mocks.saveSettings.mock.calls[1][1].closeToTray).toBe(true);
});
