import subprocess
p = subprocess.Popen(["sleep", "41"])   # e.g. a helper or worker the run starts
open("/tmp/e4.pid", "w").write(str(p.pid))
print("run exiting")
