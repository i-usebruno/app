import { invokeCmd } from '../lib/tauri';
import { useActiveEnvironmentId } from './useActiveEnvironmentId';
import { useClipboardText } from './useClipboardText';

export function useCopyAsCurl(requestId: string) {
  const [, copy] = useClipboardText();
  const environmentId = useActiveEnvironmentId();
  return async () => {
    const cmd: string = await invokeCmd('cmd_request_to_curl', { requestId, environmentId });
    copy(cmd);
    return cmd;
  };
}
