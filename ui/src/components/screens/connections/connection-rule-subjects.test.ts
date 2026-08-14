import { describe, expect, it } from 'vitest';
import type { ConnectionEntry } from '@/contracts/types';
import { connectionRuleSubjects } from './connection-rule-subjects';

const entry = (metadata: ConnectionEntry['metadata']): ConnectionEntry =>
  ({ id: 'c1', metadata }) as ConnectionEntry;

describe('connectionRuleSubjects', () => {
  it('按域名 → 目的 IP → 进程名提取，IP 不夹带端口', () => {
    expect(
      connectionRuleSubjects(
        entry({
          host: 'api.example.com',
          destinationIP: '1.1.1.1',
          destinationPort: '443',
          processPath: '/usr/bin/curl',
        }),
      ),
    ).toEqual([
      { kind: 'domain', type: 'domain', value: 'api.example.com' },
      { kind: 'ip', type: 'ipCidr', value: '1.1.1.1' },
      {
        kind: 'process',
        type: 'processName',
        value: 'curl',
        detail: '/usr/bin/curl',
      },
    ]);
  });

  it('IP 字面量不会冒充域名，Windows 路径取 basename', () => {
    expect(
      connectionRuleSubjects(
        entry({
          host: '2606:4700::1111',
          destinationIP: '2606:4700::1111',
          processPath: 'C:\\Program Files\\Browser\\browser.exe',
        }),
      ),
    ).toEqual([
      { kind: 'ip', type: 'ipCidr', value: '2606:4700::1111' },
      {
        kind: 'process',
        type: 'processName',
        value: 'browser.exe',
        detail: 'C:\\Program Files\\Browser\\browser.exe',
      },
    ]);
  });

  it('缺失或非法字段不生成死规则对象', () => {
    expect(
      connectionRuleSubjects(
        entry({ host: '—', destinationIP: 'not-an-ip', processPath: '/usr/local/bin/' }),
      ),
    ).toEqual([]);
  });
});
