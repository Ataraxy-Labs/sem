import type { SemanticParserPlugin } from './plugin.js';
import { getExtension } from '../utils/path.js';

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
