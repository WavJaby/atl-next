// SPDX-License-Identifier: MIT
package com.example.atltest;

import android.app.job.JobParameters;
import android.app.job.JobService;
import android.app.job.JobWorkItem;
import android.util.Log;

/**
 * The job service Main schedules against (app.rs SERVICE_LINES `jobscheduler`, ASYNC_LINES `jobrun`); it requires BIND_JOB_SERVICE, as JobSchedulerService.enforceValidJobRequest demands.
 * Job {@link #WORK_JOB} drains its work as androidx JobIntentService does (dequeueWork / completeWork on a worker thread until none is left) and logs what it got; any other job (Main.jobs' table job) is
 * held until its cancel stops it.
 */
public class AtlJob extends JobService {
    /** Main.jobRun's job id. */
    static final int WORK_JOB = 9;

    @Override
    public boolean onStartJob(final JobParameters params) {
        if (params.getJobId() != WORK_JOB) {
            return true;
        }
        // an anonymous class, not a lambda: the fixture's bootclasspath has no LambdaMetafactory
        new Thread(new Runnable() {
            @Override
            public void run() {
                StringBuilder sb = new StringBuilder("started=" + params.getJobId());
                try {
                    JobWorkItem w;
                    while ((w = params.dequeueWork()) != null) {
                        sb.append(" work=").append(w.getIntent().getAction()).append(" delivery=").append(w.getDeliveryCount());
                        params.completeWork(w);
                        sb.append(" completed=true");
                    }
                    sb.append(" next=null");
                } catch (Throwable t) {
                    sb.append(" threw ").append(t);
                }
                Log.i("AtlTest", "jobrun: " + sb);
            }
        }).start();
        return true;
    }

    @Override
    public boolean onStopJob(JobParameters params) {
        return false;
    }
}
