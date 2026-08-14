import type { ConnectionEntry } from '@/contracts/types';
import { validateRuleValue, type RuleSubject } from '@/domain/rules';

/** 从一条连接记录提取可复制、可建规则的对象；顺序也是右键菜单默认优先级。 */
export function connectionRuleSubjects(entry: ConnectionEntry): RuleSubject[] {
  const metadata = entry.metadata;
  if (!metadata) return [];
  const subjects: RuleSubject[] = [];
  const host = (metadata.host ?? '').trim();
  const ip = (metadata.destinationIP ?? '').trim();
  const processPath = (metadata.processPath ?? '').trim();
  const processName =
    processPath && !/[/\\]$/.test(processPath)
      ? processPath.split(/[/\\]/).filter(Boolean).pop() ?? ''
      : '';

  if (validateRuleValue('domain', host)) {
    subjects.push({ kind: 'domain', type: 'domain', value: host });
  }
  if (validateRuleValue('ipCidr', ip)) {
    subjects.push({ kind: 'ip', type: 'ipCidr', value: ip });
  }
  if (validateRuleValue('processName', processName)) {
    subjects.push({
      kind: 'process',
      type: 'processName',
      value: processName,
      detail: processPath,
    });
  }
  return subjects;
}
