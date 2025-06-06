+++
title = "Scheduler"
description = ""
date = 2024-11-09T18:10:00+00:00
updated = 2025-06-03T14:10:00+00:00
draft = false
weight = 5
sort_by = "weight"
template = "docs/page.html"

[extra]
lead = ""
toc = true
top = false
flair =[]
+++

Loco simplifies the traditional, often cumbersome `crontab` system, making it easier and more elegant to schedule cron jobs. The scheduler job can execute either a shell script command or run a registered [task](@/docs/processing/task.md).

## Setting Up

Scheduler jobs can be configured via a your YAML scheduler setup file or as part of an environment YAML file.

### 1. Your Dedicated File

Using a dedicated file provides a centralized place to configure all your scheduler jobs, making it easier to manage and maintain. You can start by generating a template file using the Loco generator command:

```sh
cargo loco generate scheduler
```

This command creates a `scheduler.yaml` file under the `config` folder. You can then configure your jobs within this file.

To use this dedicated file:

- When running the scheduler as a separate process, you'll use the `--config` flag with the `cargo loco scheduler` command (see "Verifying the Configuration" and "Running the Scheduler" sections).
- If you want the main application (started with `cargo loco start --all`) to use this dedicated file, you'll need to set the `SCHEDULER_CONFIG` environment variable (see "Running the Scheduler" section for details).

### 2. Environment Configuration File

You can also configure scheduler jobs per environment by adding the scheduler section to your environment's YAML configuration file:

<!-- <snip id="configuration-scheduler" inject_from="code" template="yaml"> -->

```yaml
scheduler:
  # Location of shipping the command stdout and stderr.
  output: stdout
  # A list of jobs to be scheduled.
  jobs:
    # The name of the job.
    write_content:
      # by default false meaning executing the the run value as a task. if true execute the run value as shell command
      shell: true
      # command to run
      run: "echo loco >> ./scheduler.txt"
      # The cron expression that defines the job's schedule.
      schedule: run every 1 second
      output: silent
      tags: ["base", "infra"]

    run_task:
      run: "foo"
      schedule: "at 10:00 am"
      run_on_start: true

    list_if_users:
      run: "user_report"
      shell: true
      schedule: "* 2 * * * *"
      tags: ["base", "users"]
```

<!-- </snip> -->

## The `SCHEDULER_CONFIG` Environment Variable

Loco uses the `SCHEDULER_CONFIG` environment variable to locate your scheduler configuration file (e.g., `scheduler.yaml`) when it's not embedded directly within your environment's main configuration file (like `development.yaml`).

This is particularly important when:

- You have a dedicated `scheduler.yaml` file (e.g., generated by `cargo loco generate scheduler`).
- You are running the scheduler as part of the main application using `cargo loco start --all`.

In such cases, you need to set `SCHEDULER_CONFIG` to the path of your scheduler file. For example:
`export SCHEDULER_CONFIG=config/scheduler.yaml`

## Scheduler Configuration

The scheduler configuration consists of the following elements:

- `scheduler.output` (Optional): Sets the default output location for all jobs.
  - `stdout:` Output to the console (default).
  - `silent:` Suppress all output.
- `scheduler.jobs:` A object of jobs to be scheduled, the object key describe the job name. Each job has:

  - `schedule`: The cron expression that defines the job's schedule.
    The cron get an english that convert to cron syntax or cron syntax itself.

    ##### **_English to cron_**

    - Examples:
    - every 15 seconds
    - run every minute
    - fire every day at 4:00 pm
    - at 10:00 am
    - run at midnight on the 1st and 15th of the month
    - On Sunday at 12:00
    - 7pm every Thursday
    - midnight on Tuesdays

    ##### **_Cron Syntax format:_**

    The cronjob should be UTC based

    ```sh
    sec   min   hour   day of month   month   day of week   year
    *     *     *      *              *       *             *
    ```

  - `run_on_start`: By default, `false`. If set to `true`, the job will also run at the start of the scheduler.
    - `shell`: by default `false` meaning executing the the `run` value as a task. if `true` execute the `run` value as shell command
    - `run`: Cronjob command to run.
      - `Task:` The task name (with variables e.x `[TASK_NAME] KEY:VAl`. follow [here](@/docs/processing/task.md) to see task arguments ). Note that the `shell` field should be false.
      - `Shell`: Run a shell command (e.x `"echo loco >> ./scheduler.txt"`). Note that the `shell` field should be true.
    - `tags` (Optional): A list of tags to categorize and manage the job.
    - `output` (Optional): Overrides the global `scheduler.output` for this job.

## Verifying the Configuration

After setting up your jobs, you can verify the configuration to ensure everything is correct.

### 1. When using a dedicated file:

Run the following command to list the jobs from your scheduler file:

<!-- <snip id="scheduler-list-from-file-command" inject_from="yaml"  template="sh"> -->

```sh
cargo loco scheduler --config config/scheduler.yaml --list
```

<!-- </snip> -->

### 2. When using environment-based configuration:

To list jobs from the environment configuration, run:

<!-- <snip id="scheduler-list-from-env-setting-command" inject_from="yaml"  template="sh"> -->

```sh
LOCO_ENV=production cargo loco scheduler --list
```

<!-- </snip> -->

This command loads the scheduler configuration from the `scheduler:` block within your `config/production.yaml` file and lists the defined jobs.

## Running the Scheduler

Once the configuration is verified, you can run the scheduler. There are two primary ways to do this:

### 1. As a Dedicated Process

This approach runs the scheduler independently of your main web server or background workers. It's useful if you want to manage its lifecycle separately.

- **Using a dedicated configuration file** (e.g., `config/scheduler.yaml`):
  Pass the path to your configuration file using the `--config` flag.

  ```sh
  cargo loco scheduler --config config/scheduler.yaml
  ```

- **Using scheduler configuration from an environment file** (e.g., `scheduler:` block in `config/development.yaml`):
  Ensure your `LOCO_ENV` is set correctly (or it defaults to `development`), then run:
  ```sh
  cargo loco scheduler
  ```
  Or, to specify an environment:
  ```sh
  LOCO_ENV=production cargo loco scheduler
  ```

### 2. Integrated with the Main Application (using `cargo loco start --all`)

This mode starts the server, background worker(s), and the scheduler together in the same process.

- Command:

  ```sh
  cargo loco start --all
  ```

- **Loading Configuration for `start --all`:**

  - **From environment configuration file**: If your scheduler settings are defined within your active environment's YAML file (e.g., under the `scheduler:` key in `config/development.yaml`), `cargo loco start --all` will automatically pick them up.
  - **From a dedicated scheduler file**: If you have a dedicated `scheduler.yaml` file (e.g., one generated by `cargo loco generate scheduler`), you **must** inform Loco where to find this file by setting the `SCHEDULER_CONFIG` environment variable.

    ```sh
    SCHEDULER_CONFIG=config/scheduler.yaml cargo loco start --all
    ```

    _This is a common point of confusion if missed. Setting `SCHEDULER_CONFIG` ensures that `cargo loco start --all` knows which scheduler configuration to load when it's not embedded in the main environment configuration file. This was highlighted in a community discussion regarding scheduler initialization failures._

The scheduler will continuously execute jobs based on their schedule until a shutdown signal (e.g., `Ctrl+C`) is received. When a signal is received, it gracefully terminates all running tasks and shuts down safely.

### Important Notes:

- When a job is running, `Loco` spawns it in a new process, and all environment variables will propagate to the new job process.
- For tasks, ensure you run the scheduler with a valid environment by using the `--environment` flag or setting the `LOCO_ENV` environment variable. This ensures the correct environment and configuration are loaded for the task.
- You can pass variables to tasks by using the vars object in the task configuration.

## Running a Single Scheduled Job by Name

To run a specific scheduler job by its name, use the --name flag. This will execute a single job with the provided name.

<!-- <snip id="scheduler-run-job-by-name-command" inject_from="yaml"  template="sh"> -->

```sh
LOCO_ENV=production cargo loco scheduler --name 'JOB_NAME'
```

<!-- </snip> -->

This command will locate the job named `"Run command"` in your scheduler.yaml file and run it.

## Running Scheduled Jobs by Tag

You can also run multiple jobs that share the same tag. Tags are useful for grouping related jobs together. For example, you might have several jobs that perform different types of maintenance tasks—such as database cleanup, cache invalidation, and log rotation—that you want to run together. Assigning them the same tag, like `maintenance`, allows you to execute them all at once.

<!-- <snip id="scheduler-run-job-by-tag-command" inject_from="yaml"  template="sh"> -->

```sh
LOCO_ENV=production cargo loco scheduler --tag 'maintenance'
```

<!-- </snip> -->

This command runs all jobs that have been tagged with `maintenance`, ensuring that all related jobs are executed in one go.
