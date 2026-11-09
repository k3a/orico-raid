# @category RE
from ghidra.app.decompiler import DecompInterface
from ghidra.util.task import ConsoleTaskMonitor
di = DecompInterface()
di.openProgram(currentProgram)
out = open(getScriptArgs()[0], "w")
fm = currentProgram.getFunctionManager()
for f in fm.getFunctions(True):
    r = di.decompileFunction(f, 120, ConsoleTaskMonitor())
    if r.decompileCompleted():
        out.write("// ==== %s @ %s\n" % (f.getName(), f.getEntryPoint()))
        out.write(r.getDecompiledFunction().getC() + "\n")
out.close()
