import { existsSync } from 'node:fs';
import { resolve } from 'node:path';
import type { SemanticParserPlugin } from './plugin.js';
import { getExtension } from '../utils/path.js';
import { detectPluginByMime } from '../utils/mime.js';

export class ParserRegistry {
  private plugins = new Map<string, SemanticParserPlugin>();
  private extensionMap = new Map<string, string>(); // ext → plugin id

  register(plugin: SemanticParserPlugin): void {
    this.plugins.set(plugin.id, plugin);
    for (const ext of plugin.extensions) {
      this.extensionMap.set(ext, plugin.id);
    }
  }

  getPlugin(filePath: string): SemanticParserPlugin | undefined {
    const ext = getExtension(filePath);
    const pluginId = this.extensionMap.get(ext);
    if (pluginId) {
      return this.plugins.get(pluginId);
    }

    // Try MIME detection via `file` command for files that exist on disk
    const absolutePath = resolve(process.cwd(), filePath);
    if (existsSync(absolutePath)) {
      const mimePluginId = detectPluginByMime(absolutePath);
      if (mimePluginId) {
        return this.plugins.get(mimePluginId);
      }
    }

    // Fallback plugin
    return this.plugins.get('fallback');
  }

  getPluginById(id: string): SemanticParserPlugin | undefined {
    return this.plugins.get(id);
  }

  listPlugins(): SemanticParserPlugin[] {
    return Array.from(this.plugins.values());
  }
}
