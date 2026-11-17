import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

export const rootDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
export const rustEnv = { ...process.env, PATH: [path.join(os.homedir(), '.cargo', 'mingw64', 'bin'), path.join(os.homedir(), '.cargo', 'bin'), process.env.PATH].join(path.delimiter) };
const cwd = path.join(rootDir, 'src-tauri');
function cargo(args, capture = false) {
  return execFileSync('cargo', args, { cwd, env: rustEnv, stdio: capture ? 'pipe' : 'inherit', maxBuffer: 64 * 1024 * 1024 })?.toString() ?? "";
}
export function runRust(args) {
  const metadata = JSON.parse(cargo(['metadata', '--locked', '--offline', '--format-version', '1', '--filter-platform', 'x86_64-pc-windows-gnu'], true));
  const target = metadata.target_directory;
  fs.mkdirSync(target, { recursive: true });
  const marker = path.join(target, '.synccloud-workspace');
  if (fs.existsSync(marker) && fs.readFileSync(marker, 'utf8') !== cwd) {
    // Cargo native build output may contain absolute paths from the old checkout.
    cargo(['clean', ...['synccloud', 'tauri', 'tauri-plugin-dialog', 'tauri-plugin-fs', 'tauri-plugin-opener', 'webview2-com-sys', 'aws-lc-rs', 'aws-lc-sys', 'libsqlite3-sys', 'ring'].flatMap(p => ['-p', p])]);
  }
  fs.writeFileSync(marker, cwd);
  const host = cargo(['-vV'], true);
  if (process.platform === 'win32' && host.includes('windows-gnu')) {
    const release = args.includes('--release');
    const profile = path.join(target, release ? 'release' : 'debug');
    cargo(['check', '--locked', '-p', 'webview2-com-sys', ...(release ? ['--release'] : [])]);
    const pkg = metadata.packages.find(p => p.name === 'webview2-com-sys');
    const source = path.join(path.dirname(pkg.manifest_path), 'x64');
    const importLib = fs.readFileSync(path.join(source, 'WebView2Loader.dll.lib'));
    let changed = false;
    for (const entry of fs.readdirSync(path.join(profile, 'build')).filter(n => n.startsWith('webview2-com-sys-'))) {
      const output = path.join(profile, 'build', entry, 'out', 'x64');
      if (!fs.existsSync(output)) continue;
      const dest = path.join(output, 'libWebView2LoaderStatic.a');
      if (!fs.existsSync(dest) || !fs.readFileSync(dest).equals(importLib)) {
        fs.writeFileSync(dest, importLib); changed = true;
      }
    }
    const deps = path.join(profile, 'deps'); fs.mkdirSync(deps, { recursive: true });
    if (changed) for (const file of fs.readdirSync(deps).filter(n => /^libwebview2_com_sys-.*\.rlib$/.test(n))) fs.unlinkSync(path.join(deps, file));
    for (const dir of [profile, deps]) fs.copyFileSync(path.join(source, 'WebView2Loader.dll'), path.join(dir, 'WebView2Loader.dll'));
  }
  cargo(args);
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) runRust(process.argv.slice(2));
