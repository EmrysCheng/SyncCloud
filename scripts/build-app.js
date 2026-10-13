import { execSync } from 'child_process';
import fs from 'fs';
import path from 'path';
import os from 'os';
import { fileURLToPath } from 'url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const rootDir = path.resolve(__dirname, '..');

const mingwBin = path.join(os.homedir(), '.cargo', 'mingw64', 'bin');
const cargoBin = path.join(os.homedir(), '.cargo', 'bin');
const env = { ...process.env, PATH: `${mingwBin};${cargoBin};${process.env.PATH}` };

console.log('=== [1/3] Building Frontend Assets ===');
execSync('npm run build', { cwd: rootDir, stdio: 'inherit', env });

console.log('\n=== [2/3] Building Tauri Core Application ===');
execSync('cargo build --release --features custom-protocol', { cwd: path.join(rootDir, 'src-tauri'), stdio: 'inherit', env });

console.log('\n=== [3/3] Building Portable Single-File Launcher ===');
execSync('cargo build --release', { cwd: path.join(rootDir, 'launcher'), stdio: 'inherit', env });

const distAppDir = path.join(rootDir, 'dist-app');
fs.mkdirSync(distAppDir, { recursive: true });

const launcherExe = path.join(rootDir, 'launcher', 'target', 'release', 'synccloud_launcher.exe');
const targetExe = path.join(distAppDir, 'synccloud.exe');
const srcDll = path.join(rootDir, 'src-tauri', 'target', 'release', 'WebView2Loader.dll');
const targetDll = path.join(distAppDir, 'WebView2Loader.dll');

fs.copyFileSync(launcherExe, targetExe);
if (fs.existsSync(srcDll)) {
  fs.copyFileSync(srcDll, targetDll);
}

const readme = `SyncCloud 独立单文件绿色版说明
====================================
1. synccloud.exe 为自包含单文件绿色版应用程序（内置完整前端资产、后端逻辑与运行库）。
2. 您可以将 synccloud.exe 随意拷贝、移动到桌面或任何独立目录，直接双击运行即可，无需同目录下附带其他文件。
3. 如果您希望保留同目录加速运行，也可将 WebView2Loader.dll 保留在同目录。
`;
fs.writeFileSync(path.join(distAppDir, 'README.txt'), readme);

const stat = fs.statSync(targetExe);
console.log(`\n[SUCCESS] Build finished successfully!`);
console.log(`Single-file executable: ${targetExe} (${(stat.size / 1024 / 1024).toFixed(2)} MB)`);
