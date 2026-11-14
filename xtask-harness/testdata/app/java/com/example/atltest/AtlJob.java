// SPDX-License-Identifier: MIT
package com.example.atltest;

import android.app.job.JobParameters;
import android.app.job.JobService;

/** The job service Main.jobs schedules against (app.rs SERVICE_LINES `jobscheduler`); it requires BIND_JOB_SERVICE, as JobSchedulerService.enforceValidJobRequest demands. Never run: the services store jobs only. */
public class AtlJob extends JobService {
    @Override
    public boolean onStartJob(JobParameters params) {
        return false;
    }

    @Override
    public boolean onStopJob(JobParameters params) {
        return false;
    }
}
