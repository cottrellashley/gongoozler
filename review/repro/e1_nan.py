import time, gongoozler as gz
gz.log("before")
gz.emit("custom", value=float("nan"))   # one bad value from user code
time.sleep(0.3)
gz.log("after")                          # does anything still arrive?
with gz.stage("later"):
    gz.progress("p", 1, 1)
