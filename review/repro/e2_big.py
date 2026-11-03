import time, gongoozler as gz
gz.log("before")
gz.log("x" * 5_000_000)                  # e.g. logging a DataFrame's repr
time.sleep(0.3)
gz.log("after")
with gz.stage("later"):
    pass
