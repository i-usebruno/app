import { useQuery } from '@tanstack/react-query';
import { invokeCmd } from '../lib/tauri';
import type { Tokens } from '@yaakapp-internal/template';

export function useParseTemplate(template: string) {
  return useQuery<Tokens>({
    queryKey: ['parse_template', template],
    queryFn: () => parseTemplate(template),
  });
}

export async function parseTemplate(template: string): Promise<Tokens> {
  return invokeCmd('cmd_parse_template', { template });
}
