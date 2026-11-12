import * as vscode from 'vscode';
import { execFile, spawn } from 'child_process';
import * as path from 'path';
import * as fs from 'fs';

// ── helpers ──────────────────────────────────────────────────────────────────

function yunpaoBin(): string {
    return vscode.workspace.getConfiguration('yunpao').get<string>('binaryPath') ?? 'yunpao';
}

function workspaceRoot(): string | undefined {
    return vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
}

function yunpaoExists(cwd: string): boolean {
    return fs.existsSync(path.join(cwd, 'yunpao.toml'));
}

function runInTerminal(args: string[], name = 'yunpao'): vscode.Terminal {
    const terminal = vscode.window.createTerminal({ name });
    terminal.show();
    terminal.sendText(`${yunpaoBin()} ${args.join(' ')}`);
    return terminal;
}

// Quick-run yunpao and capture stdout (for reading sessions/actions)
function yunpaoOut(args: string[], cwd: string): Promise<string> {
    return new Promise((resolve) => {
        execFile(yunpaoBin(), args, { cwd }, (err, stdout) => {
            resolve(err ? '' : stdout);
        });
    });
}

// ── Tree view ─────────────────────────────────────────────────────────────────

type ItemKind = 'session' | 'action' | 'task' | 'hint';

class YunpaoItem extends vscode.TreeItem {
    constructor(
        public readonly label: string,
        public readonly kind: ItemKind,
        public readonly data?: string,
        collapsible = vscode.TreeItemCollapsibleState.None,
    ) {
        super(label, collapsible);
        this.contextValue = kind === 'task' ? 'yunpaoTask' : kind;

        switch (kind) {
            case 'session':
                this.iconPath = new vscode.ThemeIcon('server');
                break;
            case 'action':
                this.iconPath = new vscode.ThemeIcon('play');
                this.command = {
                    command: 'yunpao.runAction',
                    title: 'Run',
                    arguments: [label],
                };
                break;
            case 'task':
                this.iconPath = new vscode.ThemeIcon('loading~spin');
                this.description = data ?? '';
                break;
            case 'hint':
                this.iconPath = new vscode.ThemeIcon('info');
                this.description = label;
                this.label = '';
                break;
        }
    }
}

class SessionsProvider implements vscode.TreeDataProvider<YunpaoItem> {
    private _onDidChangeTreeData = new vscode.EventEmitter<YunpaoItem | undefined | void>();
    readonly onDidChangeTreeData = this._onDidChangeTreeData.event;

    refresh(): void { this._onDidChangeTreeData.fire(); }

    getTreeItem(el: YunpaoItem) { return el; }

    async getChildren(): Promise<YunpaoItem[]> {
        const cwd = workspaceRoot();
        if (!cwd || !yunpaoExists(cwd)) {
            return [new YunpaoItem('No yunpao.toml in workspace', 'hint')];
        }
        const raw = await yunpaoOut(['session', 'list'], cwd);
        if (!raw.trim()) {
            return [new YunpaoItem('No sessions — run yunpao session new', 'hint')];
        }
        const lines = raw.split('\n').filter(l => l.includes(':'));
        return lines.map(line => {
            const isCurrent = line.includes('[current]');
            const name = line.split(':')[1]?.split('[')[0]?.trim() ?? line;
            return new YunpaoItem(
                isCurrent ? `● ${name}` : `○ ${name}`,
                'session',
                name,
            );
        });
    }
}

class ActionsProvider implements vscode.TreeDataProvider<YunpaoItem> {
    private _onDidChangeTreeData = new vscode.EventEmitter<YunpaoItem | undefined | void>();
    readonly onDidChangeTreeData = this._onDidChangeTreeData.event;

    refresh(): void { this._onDidChangeTreeData.fire(); }

    getTreeItem(el: YunpaoItem) { return el; }

    async getChildren(): Promise<YunpaoItem[]> {
        const cwd = workspaceRoot();
        if (!cwd) { return []; }
        const tomlPath = path.join(cwd, 'yunpao.toml');
        if (!fs.existsSync(tomlPath)) {
            return [new YunpaoItem('No yunpao.toml in workspace', 'hint')];
        }
        // Parse action names from yunpao.toml (simple line-based scan)
        const content = fs.readFileSync(tomlPath, 'utf-8');
        const actions: string[] = [];
        for (const line of content.split('\n')) {
            const m = line.match(/^\[actions\.([^\]]+)\]/);
            if (m) { actions.push(m[1]); }
        }
        if (!actions.length) {
            return [new YunpaoItem('No actions in yunpao.toml', 'hint')];
        }
        return actions.map(a => new YunpaoItem(a, 'action'));
    }
}

// ── Commands ──────────────────────────────────────────────────────────────────

async function cmdPush() {
    const cwd = workspaceRoot();
    if (!cwd) { vscode.window.showErrorMessage('Open a folder first'); return; }
    runInTerminal(['push'], 'yunpao push');
}

async function cmdRun() {
    const cwd = workspaceRoot();
    if (!cwd) { return; }
    const tomlPath = path.join(cwd, 'yunpao.toml');
    const content = fs.existsSync(tomlPath) ? fs.readFileSync(tomlPath, 'utf-8') : '';
    const actions: string[] = [];
    for (const line of content.split('\n')) {
        const m = line.match(/^\[actions\.([^\]]+)\]/);
        if (m) { actions.push(m[1]); }
    }
    const pick = actions.length
        ? await vscode.window.showQuickPick(actions, { placeHolder: 'Select action to run' })
        : await vscode.window.showInputBox({ prompt: 'Action name' });
    if (!pick) { return; }
    runInTerminal(['run', pick, '--watch'], `yunpao: ${pick}`);
}

async function cmdRunAction(action: string) {
    runInTerminal(['run', action, '--watch'], `yunpao: ${action}`);
}

async function cmdWatch() {
    const action = await vscode.window.showInputBox({ prompt: 'Action to watch logs for' });
    if (!action) { return; }
    runInTerminal(['watch', action], `yunpao logs: ${action}`);
}

async function cmdOpenUi() {
    runInTerminal(['ui'], 'yunpao UI');
}

async function cmdOpenTui() {
    runInTerminal(['tui'], 'yunpao TUI');
}

async function cmdAsk() {
    const request = await vscode.window.showInputBox({
        prompt: 'Describe what you want yunpao to do',
        placeHolder: 'e.g. "run training on my GPU server and watch the logs"',
    });
    if (!request) { return; }
    runInTerminal(['ask', `"${request}"`], 'yunpao AI');
}

// ── Activation ────────────────────────────────────────────────────────────────

export function activate(context: vscode.ExtensionContext) {
    const sessionsProvider = new SessionsProvider();
    const actionsProvider = new ActionsProvider();

    vscode.window.registerTreeDataProvider('yunpaoSessions', sessionsProvider);
    vscode.window.registerTreeDataProvider('yunpaoActions', actionsProvider);

    context.subscriptions.push(
        vscode.commands.registerCommand('yunpao.push', cmdPush),
        vscode.commands.registerCommand('yunpao.run', cmdRun),
        vscode.commands.registerCommand('yunpao.runAction', cmdRunAction),
        vscode.commands.registerCommand('yunpao.watch', cmdWatch),
        vscode.commands.registerCommand('yunpao.openUi', cmdOpenUi),
        vscode.commands.registerCommand('yunpao.openTui', cmdOpenTui),
        vscode.commands.registerCommand('yunpao.ask', cmdAsk),
        vscode.commands.registerCommand('yunpao.refreshSessions', () => {
            sessionsProvider.refresh();
            actionsProvider.refresh();
        }),
    );

    // Auto-refresh tree when workspace changes
    vscode.workspace.onDidSaveTextDocument(doc => {
        if (doc.fileName.endsWith('yunpao.toml')) {
            sessionsProvider.refresh();
            actionsProvider.refresh();
        }
    });
}

export function deactivate() {}
