import { InternalEvent } from '@yaakapp/api';
import { createChannel, createClient, Status } from 'nice-grpc';
import { EventChannel } from './EventChannel';
import { PluginRuntimeClient, PluginRuntimeDefinition } from './gen/plugins/runtime';
import { PluginHandle } from './PluginHandle';

const port = process.env.PORT || '50051';

const channel = createChannel(`localhost:${port}`);
const client: PluginRuntimeClient = createClient(PluginRuntimeDefinition, channel);

const events = new EventChannel();
const plugins: Record<string, PluginHandle> = {};

(async () => {
  try {
    for await (const e of client.eventStream(events.listen())) {
      const pluginEvent: InternalEvent = JSON.parse(e.event);
      // Handle special event to bootstrap plugin
      if (pluginEvent.payload.type === 'boot_request') {
        const plugin = new PluginHandle(pluginEvent.payload.dir, pluginEvent.pluginRefId, events);
        plugins[pluginEvent.pluginRefId] = plugin;
      }

      // Once booted, forward all events to the plugin worker
      const plugin = plugins[pluginEvent.pluginRefId];
      if (!plugin) {
        console.warn('Failed to get plugin for event by', pluginEvent.pluginRefId);
        continue;
      }

      if (pluginEvent.payload.type === 'terminate_request') {
        await plugin.terminate();
        console.log('Terminated plugin worker', pluginEvent.pluginRefId);
        delete plugins[pluginEvent.pluginRefId];
      }

      plugin.sendToWorker(pluginEvent);
    }
    console.log('Stream ended');
  } catch (err: any) {
    if (err.code === Status.CANCELLED) {
      console.log('Stream was cancelled by server');
    } else {
      console.log('Client stream errored', err);
    }
  }
})();
