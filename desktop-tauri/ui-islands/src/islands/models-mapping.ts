/** 新建映射的默认对外别名；编辑路径不调用此函数，因此不会改写存量 alias。 */
export function defaultMappingAlias(providerId: string, modelId: string): string {
  const provider = String(providerId || '').trim().toLowerCase()
  const prefix = ({
    workbuddy: 'workbuddy-cn',
    'workbuddy-intl': 'workbuddy-ai',
    'workbuddy-ai': 'workbuddy-ai',
    zcode: 'zcode-cn',
    'zcode-intl': 'zcode-ai',
    'zcode-ai': 'zcode-ai',
    autoclaw: 'autoclaw-cn',
    'autoclaw-intl': 'autoclaw-ai',
    'autoclaw-ai': 'autoclaw-ai',
    qoder: 'qoder-cn',
    'qoder-intl': 'qoder-ai',
    'qoder-ai': 'qoder-ai',
    monkeycode: 'monkeycode-cn',
    'monkeycode-intl': 'monkeycode-ai',
    'monkeycode-ai': 'monkeycode-ai',
    // Accio 的 provider id 方向与其它地区化提供商相反：`accio` 是国际版。
    accio: 'accio-ai',
    'accio-intl': 'accio-ai',
    'accio-ai': 'accio-ai',
    'accio-cn': 'accio-cn',
  } as Record<string, string>)[provider] || provider
  return `${prefix}/${modelId}`
}
