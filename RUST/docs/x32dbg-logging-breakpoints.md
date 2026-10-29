# Logging a game function with x32dbg

How to see every call of one function in Victoria 2, with its arguments, without stopping the game.
It uses only x32dbg's own features. You need the address of the function you are interested in;
finding it is up to you (a disassembler such as Ghidra helps).

**Use a spare copy of the game.** The breakpoints below only write to the log; nothing changes the
game's memory.

## 1. Write down what you want to log

- **FN:** the address of the function.
- The arguments you want to see, as registers or stack slots. With MSVC `thiscall` methods the
  object is in `ecx`; other arguments are on the stack, the first one at `[esp+4]` on entry.

## 2. Start the game under the debugger

1. Open `x32dbg.exe` (Victoria 2 is a 32-bit program, so use x32, not x64).
2. File → Open → `v2game.exe` from your spare copy.
3. Press **F9** until the game's launcher or menu appears.
4. Load a save and pause the game.

If the game closes or crashes on start, start it normally first, then use File → **Attach** and
pick `v2game.exe`.

## 3. Send the log to a file

Open the **Log** tab, right-click, choose **Redirect Log...**, and pick a file.

## 4. Set the logging breakpoint

In the **Command** box at the bottom, enter one line at a time:

```
bp FN
bpcond FN, 0
bplog FN, "call this={x:ecx} a1={x:[esp+4]} a2={x:[esp+8]}"
```

- `bpcond FN, 0` means the game never actually stops there.
- `bplog` writes one line to the log every time the function runs.

## 5. Run a little, then stop

1. Unpause the game, let a day or two pass, and pause again.
2. Right-click the log and choose **Stop Redirection**.
3. Close the game.

## If something goes wrong

- **The log stays empty:** the function does not run in that stretch of the game. Try its caller.
- **The game crawls:** normal with many thousands of log lines. One game day is usually enough.
