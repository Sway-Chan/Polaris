// electron-mock-hook.mjs — tsx ESM loader hook：拦截 electron import，注入 mock。
// 同 config-snapshot.test.ts:10-21 的 jest.mock('electron') 等价。
// 用法：npx tsx --import ./electron-mock-hook.mjs script.mts
export async function resolve(specifier, context, nextResolve) {
  if (specifier === 'electron') {
    // 返回 self 作 fake module URL（返回 data URL 模块）。
    return {
      url: 'data:text/javascript,' + encodeURIComponent(`
        const app = {
          getPath: () => '/fake/userData',
          getVersion: () => '9.9.9',
          isPackaged: false,
          getAppPath: () => '/fake/app',
        };
        export { app };
        export default { app };
        export const BrowserWindow = class {};
        export const Notification = class {};
        export const net = {};
        export const session = {};
      `),
      shortCircuit: true,
    };
  }
  return nextResolve(specifier, context);
}
