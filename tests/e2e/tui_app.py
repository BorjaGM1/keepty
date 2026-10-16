"""Tiny deterministic curses application for real PTY integration tests."""
import curses


def main(screen):
    curses.noecho()
    curses.cbreak()
    screen.keypad(True)
    count, key, resizes = 0, 'none', 0
    history = []
    while True:
        rows, cols = screen.getmaxyx()
        screen.erase()
        screen.addstr(0, 0, 'KEEPTY CURSES UI')
        screen.addstr(1, 0, f'count={count} key={key}')
        screen.addstr(2, 0, f'size={cols}x{rows} resizes={resizes}')
        screen.addstr(3, 0, 'log=' + ','.join(history[-8:]))
        screen.refresh()
        char = screen.getch()
        if char == ord('q'):
            return
        if char == curses.KEY_RESIZE:
            resizes += 1
        else:
            count += 1
            key = chr(char) if 32 <= char < 127 else curses.keyname(char).decode()
            history.append(key)


if __name__ == '__main__':
    curses.wrapper(main)
