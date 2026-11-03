import os, time, gongoozler as gz
open("/tmp/e3.pid", "w").write(str(os.getpid()))
with gz.stage("long"):
    time.sleep(20)
