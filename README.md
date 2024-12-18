### Thread control algroithm
Split the process into 2 parts, slow start & max thread control

#### Slow start
Start with minimum thread 1, then for each time period, split each thread into half. Stop the process when more thread can't get us significant boost. The $THREASHOLD_1$ should be large, since this process is just rough estimate.
$$
thread_t = 2\times thread_{t-1} \\
|total\_speed_t - 2\times total\_speed_{t-1}| < THREASHOLD_1
$$

#### Max thread control
When we reached $THREASHOLD_1$, instead of spliting every thread, choose one task that has the longest unloaded range, then split the remaining chunk into to tasks.
$$
thread_t = thread_{t-1} \\
|total\_speed_t - (total\_speed_{t-1} + average\_speed)| > THREASHOLD_2 \\
thread_t < MAX\_THREADS
$$
