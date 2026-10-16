"""Real curses and vim applications exercised through CLI and SDK clients."""
import os
import re
import shutil
import sys
from support import AGENT_HOME, ROOT, Role, poll, raw_until


class Skip(Exception):
    pass


def curses_case(lab):
    sid, pid = lab.start([sys.executable, str(ROOT / 'tests/e2e/tui_app.py')])
    lab.writer(sid)
    lab.screen(sid, 'KEEPTY CURSES UI')
    lab.wait(sid, 'count=0')
    lab.cli('send', sid, 'ab')
    lab.wait(sid, 'count=2 key=b')
    waiter = lab.armed(sid, 'count=3')
    lab.cli('keys', sid, 'enter')
    assert waiter.wait(timeout=30) == 0
    lab.screen(sid, 'count=3 key=^J')
    with lab.connect(sid, Role.WRITER, cols=101, rows=31):
        lab.wait(sid, 'size=101x31')
        poll(lambda: re.search(r'resizes=[1-9][0-9]*', lab.cli('screen', sid).stdout),
             'app handled KEY_RESIZE')
    # All Writer/Monitor connections have now disconnected. Reconnect a Monitor
    # and verify its replay with the in-repo SDK and CLI screen parser.
    with lab.connect(sid) as monitor:
        replay = raw_until(monitor, b'count=3')
        assert b'KEEPTY CURSES UI' in replay
    lab.screen(sid, 'count=3')
    history = lab.cli('screen', sid, '--scrollback').stdout
    assert 'count=3' in history and 'size=101x31' in history
    lab.stop(sid, pid)


def curses_quit(lab):
    sid, pid = lab.start([sys.executable, str(ROOT / 'tests/e2e/tui_app.py')])
    lab.writer(sid)
    lab.wait(sid, 'count=0')
    result = lab.cli('send', sid, 'q', check=False)
    assert result.returncode in (0, 3), f'send q: {result}'
    lab.gone(sid, pid)


def vim_case(lab):
    vim = shutil.which('vim')
    if not vim:
        raise Skip('vim is not on PATH')
    path = lab.directory / 'edited.txt'
    sid, pid = lab.start([vim, '-Nu', 'NONE', '-i', 'NONE', '-n', str(path)])
    lab.writer(sid)
    lab.screen(sid, 'edited.txt')
    with lab.connect(sid, Role.WRITER) as writer:
        writer.send_keys(b'i')
        lab.screen(sid, '-- INSERT --')
    lab.cli('send', sid, 'hello from keepty')
    lab.screen(sid, 'hello from keepty')
    lab.cli('keys', sid, 'escape')
    # The acknowledgement can race with normal child exit; send the final Enter
    # via a retained Writer, then observe the file and broker exit explicitly.
    with lab.connect(sid, Role.WRITER) as writer:
        writer.send_keys(b':wq\r')
        poll(lambda: path.exists() and path.read_text() == 'hello from keepty\n', 'vim saved file')
    lab.gone(sid, pid)


def agent_case(lab):
    if os.environ.get('KEEPTY_E2E_AGENTS') != '1':
        raise Skip('set KEEPTY_E2E_AGENTS=1 to run codex (requires configured credentials)')
    codex = shutil.which('codex')
    if not codex:
        raise Skip('codex is not on PATH')
    # HOME is isolated by the runner; retain the explicitly enabled agent's
    # configured credential directory. Normal test runs never launch the agent.
    previous = os.environ.get('CODEX_HOME')
    os.environ['CODEX_HOME'] = AGENT_HOME
    try:
        sid, pid = lab.start([codex, 'exec', '--skip-git-repo-check',
                              'Reply with exactly KEEPTY_AGENT_OK. Do not use any tools.'])
    finally:
        if previous is None:
            os.environ.pop('CODEX_HOME', None)
        else:
            os.environ['CODEX_HOME'] = previous
    monitor = lab.connect(sid)
    lab.writer(sid)
    raw_until(monitor, b'KEEPTY_AGENT_OK')
    lab.gone(sid, pid)


def cases():
    return [('tui/curses', curses_case), ('tui/curses_quit', curses_quit),
            ('tui/vim', vim_case), ('agents/codex', agent_case)]
