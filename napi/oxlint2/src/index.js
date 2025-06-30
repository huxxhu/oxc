import { lint } from './bindings.js';

class Linter {
  pluginRegistry = new Map();

  run() {
    return lint(this.loadPlugin.bind(this), this.lint.bind(this));
  }

  loadPlugin = async (pluginName) => {
    if (this.pluginRegistry.has(pluginName)) {
      return { type: 'Success' };
    }

    try {
      const plugin = await import(pluginName);
      pluginRegistry.set(pluginName, plugin);
      return { type: 'Success' };
    } catch (error) {
      const errorMessage = 'message' in error && typeof error.message === 'string'
        ? error.message
        : 'An unknown error occurred';
      return { type: 'Failure', field0: errorMessage };
    }
  };

  lint = async () => {
    throw new Error('unimplemented');
  };
}

function main() {
  const linter = new Linter();

  const result = linter.run();

  if (!result) {
    process.exit(1);
  }
}

main();
