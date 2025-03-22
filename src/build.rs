use crate::cross_compile::{get_cross_compilation_env, setup_cross_compilation};
use std::collections::{HashMap, HashSet};
use std::{fs, thread};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use colored::Colorize;
use crate::config::{BuildProgressState, ProjectConfig, VariantSettings, WorkspaceConfig};
use crate::{categorize_error, ensure_build_tools, ensure_compiler_available, get_effective_compiler_label, has_command, is_msvc_style_for_config, map_compiler_label, parse_universal_flags, print_general_suggestions, run_command_with_timeout};
use crate::cross_compile::get_predefined_cross_target;
use crate::dependencies::install_dependencies;
use crate::errors::{format_compiler_errors, glob_to_regex};
use crate::output_utils::{is_quiet, is_verbose, print_status, print_substep, print_warning, BuildProgress, ProgressBar};
use crate::project::generate_cmake_lists;
use crate::workspace::resolve_workspace_dependencies;

pub fn configure_project(
    config: &ProjectConfig,
    project_path: &Path,
    config_type: Option<&str>,
    variant_name: Option<&str>,
    cross_target: Option<&str>,
    workspace_config: Option<&WorkspaceConfig>
) -> Result<(), Box<dyn std::error::Error>> {
    // Create a progress tracker for configuration
    let mut progress = BuildProgress::new(&format!("Configuring {}", config.project.name), 5);

    // Step 1: Ensure tools
    progress.next_step("Checking required tools");
    ensure_build_tools(config)?;

    // Get compiler and build paths
    let compiler_label = get_effective_compiler_label(config);
    let build_dir = config.build.build_dir.as_deref().unwrap_or("build");
    let build_path = if let Some(target) = cross_target {
        project_path.join(format!("{}-{}", build_dir, target))
    } else {
        project_path.join(build_dir)
    };
    fs::create_dir_all(&build_path)?;

    // Create environment for hooks
    let mut hook_env = HashMap::new();
    hook_env.insert("PROJECT_PATH".to_string(), project_path.to_string_lossy().to_string());
    hook_env.insert("BUILD_PATH".to_string(), build_path.to_string_lossy().to_string());
    hook_env.insert("CONFIG_TYPE".to_string(), get_build_type(config, config_type));

    if let Some(v) = variant_name {
        hook_env.insert("VARIANT".to_string(), v.to_string());
    }

    // Step 2: Run pre-configure hooks
    progress.next_step("Running pre-configure hooks");
    if let Some(hooks) = &config.hooks {
        if let Some(pre_hooks) = &hooks.pre_configure {
            if !pre_hooks.is_empty() {
                run_hooks(&Some(pre_hooks.clone()), project_path, Some(hook_env.clone()))?;
            }
        }
    }

    // Step 3: Setup dependencies
    progress.next_step("Setting up dependencies");

    // Check for cross-compilation config
    let cross_config = if let Some(target) = cross_target {
        if let Some(predefined) = get_predefined_cross_target(target) {
            Some(predefined)
        } else if let Some(config_cross) = &config.cross_compile {
            if config_cross.enabled && config_cross.target == target {
                Some(config_cross.clone())
            } else {
                None
            }
        } else {
            None
        }
    } else if let Some(config_cross) = &config.cross_compile {
        if config_cross.enabled {
            Some(config_cross.clone())
        } else {
            None
        }
    } else {
        None
    };

    // If cross-compiling, adjust build path
    let build_path = if let Some(cross_config) = &cross_config {
        let target_build_dir = format!("{}-{}", build_dir, cross_config.target);
        let target_build_path = project_path.join(&target_build_dir);
        fs::create_dir_all(&target_build_path)?;

        if !is_quiet() {
            print_substep(&format!("Using cross-compilation target: {}", cross_config.target));
        }

        target_build_path
    } else {
        fs::create_dir_all(&build_path)?;
        build_path
    };

    // Setup dependencies - use a single progress bar for this
    let deps_result = install_dependencies(config, project_path, false)?;

    let vcpkg_toolchain = deps_result.get("vcpkg_toolchain").cloned().unwrap_or_default();
    let conan_cmake = deps_result.get("conan_cmake").cloned().unwrap_or_default();

    // Step 4: Generate CMake files
    progress.next_step("Generating build files");

    let mut cmake_spinner = ProgressBar::start("Generating CMakeLists.txt");
    generate_cmake_lists(config, project_path, variant_name, workspace_config)?;
    cmake_spinner.success();

    // Step 5: Run CMake configuration
    progress.next_step("Running CMake configuration");

    // Get CMake generator
    let generator = get_cmake_generator(config)?;

    // Build CMake command
    let mut cmd = vec!["cmake".to_string(), "..".to_string()];

    // Add generator
    cmd.push("-G".to_string());
    cmd.push(generator.clone());

    // Add build type
    let build_type = get_build_type(config, config_type);
    cmd.push(format!("-DCMAKE_BUILD_TYPE={}", build_type));

    // Add vcpkg toolchain if available
    if !vcpkg_toolchain.is_empty() {
        cmd.push(format!("-DCMAKE_TOOLCHAIN_FILE={}", vcpkg_toolchain));
    }

    // Add compiler specification
    if let Some((c_comp, cxx_comp)) = map_compiler_label(&compiler_label) {
        cmd.push(format!("-DCMAKE_C_COMPILER={}", c_comp));
        cmd.push(format!("-DCMAKE_CXX_COMPILER={}", cxx_comp));
    }

    // Add platform-specific options
    let platform_options = get_platform_specific_options(config);
    cmd.extend(platform_options);

    // Add configuration-specific options
    let config_options = get_config_specific_options(config, &build_type);
    cmd.extend(config_options);

    // Add variant-specific options
    if let Some(variant) = get_active_variant(config, variant_name) {
        apply_variant_settings(&mut cmd, variant, config);
    }

    // Add cross-compilation options
    let mut env_vars = None;
    if let Some(cross_config) = &cross_config {
        // Get cross-compilation CMake options
        let cross_options = setup_cross_compilation(config, cross_config)?;
        cmd.extend(cross_options);

        // Get environment variables for cross-compilation
        let cross_env = get_cross_compilation_env(cross_config);
        if !cross_env.is_empty() {
            let mut all_env = cross_env;
            for (k, v) in hook_env.clone() {
                all_env.insert(k, v);
            }
            env_vars = Some(all_env);
        } else {
            env_vars = Some(hook_env.clone());
        }
    } else {
        env_vars = Some(hook_env.clone());
    }

    // Add custom CMake options
    if let Some(cmake_options) = &config.build.cmake_options {
        cmd.extend(cmake_options.clone());
    }

    // Add workspace dependency options
    if let Some(workspace) = workspace_config {
        let workspace_options = resolve_workspace_dependencies(config, Some(workspace), project_path)?;
        cmd.extend(workspace_options);
    }

    // Run the CMake configuration command with our modified silent runner
    let mut cmake_progress = ProgressBar::start("Running cmake");
    let cmake_result = run_cmake_silently(cmd.clone(), &build_path, env_vars.clone())?;
    cmake_progress.success();

    // Run post-configure hooks
    if let Some(hooks) = &config.hooks {
        if let Some(post_hooks) = &hooks.post_configure {
            if !post_hooks.is_empty() {
                print_substep("Running post-configure hooks");
                run_hooks(&Some(post_hooks.clone()), project_path, env_vars)?;
            }
        }
    }

    // Complete progress
    progress.complete();

    // Show configuration summary
    if !is_quiet() {
        print_status(&format!("Project configured with generator: {} ({})",
                              generator, build_type));

        if let Some(variant_name) = variant_name {
            print_substep(&format!("Using build variant: {}", variant_name));
        }

        if let Some(cross_config) = &cross_config {
            print_substep(&format!("Cross-compilation target: {}", cross_config.target));
        }
    }

    Ok(())
}

fn run_cmake_silently(
    cmd: Vec<String>,
    build_path: &Path,
    env_vars: Option<HashMap<String, String>>
) -> Result<(), Box<dyn std::error::Error>> {
    use std::process::{Command, Stdio};
    use std::io::{BufRead, BufReader};
    use std::time::Duration;

    // Collect error messages to display if cmake fails
    let error_messages = Arc::new(Mutex::new(Vec::new()));
    let error_messages_stdout = Arc::clone(&error_messages);
    let error_messages_stderr = Arc::clone(&error_messages);

    // Build the Command
    let mut command = Command::new(&cmd[0]);
    command.args(&cmd[1..]);
    command.current_dir(build_path);

    // Add environment variables if provided
    if let Some(env) = env_vars {
        for (key, value) in env {
            command.env(key, value);
        }
    }

    // Pipe stdout and stderr so we can read them but not display them
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    // Spawn the command
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Err(format!("Failed to start CMake: {}", e).into());
        }
    };

    // Take ownership of stdout/stderr handles
    let stdout = child.stdout.take().ok_or("Failed to capture stdout")?;
    let stderr = child.stderr.take().ok_or("Failed to capture stderr")?;

    // Track cmake progress for real-time status updates (if needed)
    let progress_tracker = Arc::new(Mutex::new(String::new()));
    let progress_tracker_clone = Arc::clone(&progress_tracker);

    // Thread for reading stdout
    let stdout_handle = thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().filter_map(Result::ok) {
            // Save status lines
            if line.contains("Configuring") || line.contains("Generating") {
                let mut current = progress_tracker_clone.lock().unwrap();
                *current = line.trim().to_string();
            }

            // Save important error messages
            if line.contains("error:") || line.contains("Error:") || line.contains("CMake Error") ||
                line.contains("WARNING:") || line.contains("fatal error") {
                println!("{}", line);  // Display in real-time
                let mut msgs = error_messages_stdout.lock().unwrap();
                msgs.push(line);
            }
        }
    });

    // Thread for reading stderr
    let stderr_handle = thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().filter_map(Result::ok) {
            // Always capture stderr lines for error reporting
            let mut msgs = error_messages_stderr.lock().unwrap();
            msgs.push(line.clone());

            // Display error lines in real-time
            if line.contains("error:") || line.contains("Error:") || line.contains("CMake Error") ||
                line.contains("WARNING:") || line.contains("fatal error") {
                eprintln!("{}", line.red());
            }
        }
    });

    // Wait for the command to complete with a reasonable timeout
    let timeout = Duration::from_secs(600); // 10 minute timeout for CMake
    let (tx, rx) = std::sync::mpsc::channel();

    thread::spawn(move || {
        let status = child.wait();
        let _ = tx.send(status);
    });

    match rx.recv_timeout(timeout) {
        Ok(status_result) => {
            match status_result {
                Ok(status) => {
                    if !status.success() {
                        // Wait for stdout/stderr readers to finish to get all messages
                        let _ = stdout_handle.join();
                        let _ = stderr_handle.join();

                        // Get collected error messages
                        let error_msgs = error_messages.lock().unwrap();

                        // If we have error messages, display them
                        if !error_msgs.is_empty() {
                            println!("\n{}", "CMake configuration failed with these errors:".red().bold());
                            for msg in error_msgs.iter().take(20) {  // Limit to 20 messages
                                println!("  {}", msg);
                            }

                            // Provide some common solutions
                            println!("\n{}", "Possible solutions:".yellow().bold());
                            println!("  • Make sure you have the correct compiler installed");
                            println!("  • Check if your C++ standard is supported by your compiler");
                            println!("  • Verify that all dependencies are properly installed");
                            println!("  • Try 'cforge clean' and then build again");
                            println!("  • Run with verbose output: set CFORGE_VERBOSE=1");
                        }

                        return Err(format!("CMake configuration failed with exit code: {}", status).into());
                    }
                },
                Err(e) => return Err(format!("Command error: {}", e).into()),
            }
        },
        Err(_) => {
            return Err(format!("CMake configuration timed out after {} seconds", timeout.as_secs()).into());
        }
    }

    // Wait for stdout/stderr readers to finish
    let _ = stdout_handle.join();
    let _ = stderr_handle.join();

    Ok(())
}

pub fn execute_build_with_progress(
    cmd: Vec<String>,
    build_path: &Path,
    source_files_count: usize,
    mut progress: ProgressBar
) -> Result<(), Box<dyn std::error::Error>> {
    use std::process::{Command, Stdio};
    use std::io::{BufRead, BufReader};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    // Check if this is a CMake command
    let is_cmake_command = cmd.len() > 0 && cmd[0].contains("cmake");

    // Build the Command
    let mut command = Command::new(&cmd[0]);
    command.args(&cmd[1..]);
    command.current_dir(build_path);

    // Pipe stdout and stderr so we can read them
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    // Initial progress update - show we're starting
    progress.update(0.01);

    // Spawn the command
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            progress.failure(&format!("Failed to start build: {}", e));
            return Err(format!("Failed to start build command: {}", e).into());
        }
    };

    // Take ownership of stdout/stderr handles
    let stdout = child.stdout.take().ok_or("Failed to capture stdout")?;
    let stderr = child.stderr.take().ok_or("Failed to capture stderr")?;

    // Shared state for tracking build progress
    let build_state = Arc::new(Mutex::new(BuildProgressState {
        compiled_files: 0,
        total_files: source_files_count.max(1), // Ensure we don't divide by zero
        current_percentage: 0.0,
        errors: Vec::new(),
        is_linking: false,
    }));

    // Buffers to collect stdout and stderr for error analysis
    let stdout_buffer = Arc::new(Mutex::new(String::new()));
    let stderr_buffer = Arc::new(Mutex::new(String::new()));

    // Create completion flags to detect when reading is complete
    let stdout_done = Arc::new(Mutex::new(false));
    let stderr_done = Arc::new(Mutex::new(false));

    // Clones for threads
    let build_state_stdout = Arc::clone(&build_state);
    let build_state_stderr = Arc::clone(&build_state);
    let stdout_done_clone = Arc::clone(&stdout_done);
    let stderr_done_clone = Arc::clone(&stderr_done);
    let stdout_buffer_clone = Arc::clone(&stdout_buffer);
    let stderr_buffer_clone = Arc::clone(&stderr_buffer);

    // Thread for reading stdout
    let stdout_handle = thread::spawn(move || {
        let reader = BufReader::new(stdout);

        for line in reader.lines().filter_map(Result::ok) {
            // Update progress based on stdout patterns
            update_build_progress(&build_state_stdout, &line, false);

            // Append to buffer for error analysis
            {
                let mut buffer = stdout_buffer_clone.lock().unwrap();
                buffer.push_str(&line);
                buffer.push('\n');
            }

            // We only show verbose output in verbose mode - we'll format errors later
            if is_verbose() {
                println!("{}", line);
            }
        }

        // Mark stdout reading as complete
        *stdout_done_clone.lock().unwrap() = true;
    });

    // Thread for reading stderr
    let stderr_handle = thread::spawn(move || {
        let reader = BufReader::new(stderr);

        for line in reader.lines().filter_map(Result::ok) {
            // Update progress based on stderr patterns
            update_build_progress(&build_state_stderr, &line, true);

            // Append to buffer for error analysis
            {
                let mut buffer = stderr_buffer_clone.lock().unwrap();
                buffer.push_str(&line);
                buffer.push('\n');
            }

            // We'll collect all errors to format them later - only show in verbose mode now
            if is_verbose() {
                eprintln!("{}", line.red());
            }
        }

        // Mark stderr reading as complete
        *stderr_done_clone.lock().unwrap() = true;
    });

    // Create a thread to update the progress bar based on the build state
    let build_state_progress = Arc::clone(&build_state);
    let stdout_done_progress = Arc::clone(&stdout_done);
    let stderr_done_progress = Arc::clone(&stderr_done);
    let progress_clone = progress.clone();
    let progress_handle = thread::spawn(move || {
        let mut last_progress = 0.0;

        // Keep updating until both stdout and stderr are done OR we reach 100%
        while !(*stdout_done_progress.lock().unwrap() && *stderr_done_progress.lock().unwrap()) {
            // Get current state
            let state = build_state_progress.lock().unwrap();

            // Calculate progress percentage
            let mut progress_value = 0.0;

            if state.is_linking {
                // If we're linking, assume we're at least 80% done
                progress_value = 0.8 + (state.current_percentage / 100.0) * 0.2;
            } else if state.total_files > 0 {
                // Otherwise base on compiled files
                let files_ratio = state.compiled_files as f32 / state.total_files as f32;
                progress_value = (files_ratio * 0.8).min(0.8); // Cap at 80% until linking starts
            }

            // Only update if progress has changed meaningfully
            if (progress_value - last_progress).abs() > 0.005 {
                progress_clone.update(progress_value);
                last_progress = progress_value;
            }

            // Release the lock before sleeping
            drop(state);

            // Don't spin the CPU - check every 100ms
            thread::sleep(Duration::from_millis(100));
        }

        // One final update to ensure we show progress
        let state = build_state_progress.lock().unwrap();
        if state.current_percentage >= 100.0 {
            progress_clone.update(1.0);
        }
    });

    // Wait for the command to complete (with watchdog to prevent hanging)
    let completed;
    let start_time = std::time::Instant::now();
    let timeout = Duration::from_secs(7200); // 2 hour timeout

    // Use a separate thread to wait for the process to exit
    let (tx, rx) = std::sync::mpsc::channel();
    let wait_handle = thread::spawn(move || {
        let status = child.wait();
        let _ = tx.send(status);
    });

    // Wait for completion with timeout
    completed = match rx.recv_timeout(timeout) {
        Ok(status_result) => {
            match status_result {
                Ok(status) => status.success(),
                Err(_) => false
            }
        },
        Err(_) => {
            // Timeout occurred
            print_warning(&format!("Build process timed out after {:?}", timeout),
                          Some("The build may still be running in the background"));
            false
        }
    };

    // Wait for stdout/stderr readers to finish
    let _ = stdout_handle.join();
    let _ = stderr_handle.join();

    // Wait for progress updater to finish, but with timeout
    let _ = progress_handle.join();

    // Get any errors that might have occurred
    let errors = {
        let state = build_state.lock().unwrap();
        state.errors.clone()
    };

    // Get the collected stdout and stderr content for error analysis if needed
    if !completed {
        let stdout_content = stdout_buffer.lock().unwrap().clone();
        let stderr_content = stderr_buffer.lock().unwrap().clone();

        // Build failed - display formatted errors using our enhanced error formatter
        progress.failure("Build failed");

        // Use our enhanced error formatter
        println!();  // Add some space
        let formatted_errors = format_compiler_errors(&stdout_content, &stderr_content);
        for error_line in &formatted_errors {
            println!("{}", error_line);
        }

        return Err("Build process failed - see above for detailed errors".into());
    }

    // Build succeeded
    progress.update(1.0);
    progress.success();
    Ok(())
}

pub fn get_build_type(config: &ProjectConfig, requested_config: Option<&str>) -> String {
    // If a specific configuration was requested, use that
    if let Some(requested) = requested_config {
        return requested.to_string();
    }

    // Otherwise use the default configuration from the config file
    if let Some(default_config) = &config.build.default_config {
        return default_config.clone();
    }

    // Fallback to traditional debug/release
    if config.build.debug.unwrap_or(true) {
        "Debug".to_string()
    } else {
        "Release".to_string()
    }
}

pub fn get_cmake_generator(config: &ProjectConfig) -> Result<String, Box<dyn std::error::Error>> {
    let generator = config.build.generator.as_deref().unwrap_or("default");

    // If VS is explicitly requested with a version number
    if generator.starts_with("Visual Studio ") || generator.to_lowercase().starts_with("vs") {
        let requested_version = if generator.to_lowercase().starts_with("vs") {
            // Extract version from "vs2019" or similar format
            let version_str = generator.trim_start_matches("vs").trim_start_matches("VS");
            match version_str {
                "2022" => Some("17 2022"),
                "2019" => Some("16 2019"),
                "2017" => Some("15 2017"),
                "2015" => Some("14 2015"),
                "2013" => Some("12 2013"),
                _ => None,
            }
        } else {
            // Extract version from "Visual Studio XX YYYY" format
            Some(generator.trim_start_matches("Visual Studio "))
        };

        return Ok(get_visual_studio_generator(requested_version));
    }

    // Handle other specific generators
    if generator != "default" {
        // If a specific generator is requested, try to ensure its tools are available
        match generator {
            "Ninja" => {
                if !has_command("ninja") {
                    ensure_compiler_available("ninja")?;
                }
            },
            "NMake Makefiles" => {
                if !has_command("nmake") {
                    ensure_compiler_available("msvc")?;
                }
            },
            "MinGW Makefiles" => {
                if !has_command("mingw32-make") && !has_command("make") {
                    ensure_compiler_available("gcc")?;
                }
            },
            _ => {} // Other generators we don't try to auto-install
        }
        return Ok(generator.to_string());
    }

    // Auto-detect based on platform
    if cfg!(target_os = "windows") {
        // On Windows, prefer Visual Studio if available
        if Command::new("cl").arg("/?").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok() {
            // Try to determine VS version
            return Ok(get_visual_studio_generator(None));
        }

        // Fallback to Ninja if available
        if Command::new("ninja").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok() {
            return Ok("Ninja".to_string());
        }

        // Default to NMake
        return Ok("NMake Makefiles".to_string());
    } else if cfg!(target_os = "macos") {
        // macOS - prefer Ninja, fallback to Xcode
        if Command::new("ninja").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok() {
            return Ok("Ninja".to_string());
        }
        return Ok("Xcode".to_string());
    } else {
        // Linux - prefer Ninja, fallback to Unix Makefiles
        if Command::new("ninja").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok() {
            return Ok("Ninja".to_string());
        }
        return Ok("Unix Makefiles".to_string());
    }
}

pub fn run_hooks(hooks: &Option<Vec<String>>, project_path: &Path, env_vars: Option<HashMap<String, String>>) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(commands) = hooks {
        for cmd_str in commands {
            println!("{}", format!("Running hook: {}", cmd_str).blue());

            // Use the system shell instead of direct command execution
            let (shell, shell_arg) = if cfg!(windows) {
                ("cmd", "/C")
            } else {
                ("sh", "-c")
            };

            // Create command
            let mut command = Command::new(shell);
            command.arg(shell_arg).arg(cmd_str);
            command.current_dir(project_path);

            // Add environment variables if provided
            if let Some(env) = &env_vars {
                for (key, value) in env {
                    command.env(key, value);
                }
            }

            // Hide detailed output
            command.stdout(Stdio::null());
            command.stderr(Stdio::piped());

            // Execute the command
            let output = command.output()?;

            // Only print errors
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                println!("{}", "Hook command failed:".red());
                if !stderr.is_empty() {
                    eprintln!("{}", stderr);
                }

                return Err(format!("Hook failed with exit code: {}", output.status).into());
            }
        }
    }

    Ok(())
}

pub fn update_build_progress(state: &Arc<Mutex<BuildProgressState>>, line: &str, is_stderr: bool) {
    let mut state = state.lock().unwrap();

    // Check if this is a compiler line showing a file being compiled
    // Improved detection of file compilation
    if (line.contains(".cpp") || line.contains(".cc") || line.contains(".c")) &&
        (line.contains("Compiling") || line.contains("Building") ||
            line.contains("C++") || line.contains("CC") || line.contains("[") ||
            line.contains("Building CXX object") || line.contains("Building C object")) {
        state.compiled_files += 1;

        // Update percentage based on files compiled
        if state.total_files > 0 {
            state.current_percentage = (state.compiled_files as f32 / state.total_files as f32) * 100.0;
        }
    }

    // More robust detection of linking phase
    if line.contains("Linking") || line.contains("Generating library") ||
        line.contains("Building executable") || line.contains("Building shared library") ||
        line.contains("Building static library") || line.contains("Linking CXX") {
        state.is_linking = true;
        state.current_percentage = 90.0;
    }

    // Check for error messages
    if (is_stderr && (line.contains("error") || line.contains("Error"))) ||
        (line.contains("fatal error") || line.contains("undefined reference")) {
        state.errors.push(line.to_string());
    }

    // Look for percentage indicators
    if let Some(percent_pos) = line.find('%') {
        if percent_pos > 0 && percent_pos < line.len() - 1 {
            let start = line[..percent_pos].rfind(|c: char| !c.is_digit(10) && c != '.').map_or(0, |pos| pos + 1);
            if let Ok(percentage) = line[start..percent_pos].trim().parse::<f32>() {
                if percentage > state.current_percentage {
                    state.current_percentage = percentage;
                }
            }
        }
    }

    // Check for build completion keywords
    if line.contains("Built target") || line.contains("Built all targets") ||
        line.contains("[100%]") || line.contains("build succeeded") {
        state.current_percentage = 100.0;
    }
}

pub fn count_project_source_files(config: &ProjectConfig, project_path: &Path) -> Result<usize, Box<dyn std::error::Error>> {
    let mut total_count = 0;

    // Process each target and its source patterns
    for (_, target) in &config.targets {
        for source_pattern in &target.sources {
            // Skip empty patterns
            if source_pattern.is_empty() {
                continue;
            }

            // Convert glob pattern to regex
            let regex_pattern = glob_to_regex(source_pattern);
            let regex = match regex::Regex::new(&regex_pattern) {
                Ok(r) => r,
                Err(_) => continue, // Skip invalid patterns
            };

            // Count files recursively
            let files_count = count_matching_files(project_path, &regex)?;
            total_count += files_count;

            // Debug output to see what's being counted
            if is_verbose() {
                println!("Pattern '{}' matched {} files", source_pattern, files_count);
            }
        }
    }

    // If we didn't find any source files, assume at least a minimal default
    if total_count == 0 {
        // Instead of returning 0, return a minimum of 1 source file
        // to avoid the "Compiling 0 source files" error
        total_count = 1;

        if is_verbose() {
            println!("No source files found with configured patterns, assuming minimal project");
        }
    }

    Ok(total_count)
}

pub fn count_matching_files(dir: &Path, regex: &regex::Regex) -> Result<usize, Box<dyn std::error::Error>> {
    let mut count = 0;

    if !dir.exists() || !dir.is_dir() {
        return Ok(0);
    }

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            // Recursively count files in subdirectories
            count += count_matching_files(&path, regex)?;
        } else if path.is_file() {
            // Check if this file matches the pattern
            if let Some(path_str) = path.to_str() {
                if regex.is_match(path_str) {
                    count += 1;
                }
            }
        }
    }

    Ok(count)
}

pub fn get_config_specific_options(config: &ProjectConfig, build_type: &str) -> Vec<String> {
    let mut options = Vec::new();

    if let Some(configs) = &config.build.configs {
        if let Some(cfg_settings) = configs.get(build_type) {
            if let Some(defines) = &cfg_settings.defines {
                for define in defines {
                    options.push(format!("-D{}=1", define));
                }
            }

            // 2) parse universal tokens for flags
            let is_msvc_style = is_msvc_style_for_config(config);

            if let Some(token_list) = &cfg_settings.flags {
                let real_flags = parse_universal_flags(token_list, is_msvc_style);
                if !real_flags.is_empty() {
                    let joined = real_flags.join(" ");
                    if is_msvc_style {
                        options.push(format!(
                            "-DCMAKE_CXX_FLAGS_{}:STRING={}",
                            build_type.to_uppercase(),
                            joined
                        ));
                        options.push(format!(
                            "-DCMAKE_C_FLAGS_{}:STRING={}",
                            build_type.to_uppercase(),
                            joined
                        ));
                    } else {
                        options.push(format!(
                            "-DCMAKE_CXX_FLAGS_{}='{}'",
                            build_type.to_uppercase(),
                            joined
                        ));
                        options.push(format!(
                            "-DCMAKE_C_FLAGS_{}='{}'",
                            build_type.to_uppercase(),
                            joined
                        ));
                    }
                }
            }

            // 3) handle link_flags, cmake_options, etc. as before
            if let Some(link_flags) = &cfg_settings.link_flags {
                if !link_flags.is_empty() {
                    let link_str = link_flags.join(" ");
                    if cfg!(windows) {
                        options.push(format!(
                            "-DCMAKE_EXE_LINKER_FLAGS_{}=\"{}\"",
                            build_type.to_uppercase(),
                            link_str
                        ));
                        options.push(format!(
                            "-DCMAKE_SHARED_LINKER_FLAGS_{}=\"{}\"",
                            build_type.to_uppercase(),
                            link_str
                        ));
                    } else {
                        options.push(format!(
                            "-DCMAKE_EXE_LINKER_FLAGS_{}='{}'",
                            build_type.to_uppercase(),
                            link_str
                        ));
                        options.push(format!(
                            "-DCMAKE_SHARED_LINKER_FLAGS_{}='{}'",
                            build_type.to_uppercase(),
                            link_str
                        ));
                    }
                }
            }
            if let Some(cmake_opts) = &cfg_settings.cmake_options {
                options.extend(cmake_opts.clone());
            }
        }
    }
    options
}

pub fn get_platform_specific_options(config: &ProjectConfig) -> Vec<String> {
    let current_os = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };

    let mut options = Vec::new();

    if let Some(platforms) = &config.platforms {
        if let Some(platform_config) = platforms.get(current_os) {
            // Add platform-specific defines with =1 format
            if let Some(defines) = &platform_config.defines {
                for define in defines {
                    options.push(format!("-D{}=1", define));
                }
            }

            // Add platform-specific flags with proper quoting
            if let Some(flags) = &platform_config.flags {
                if !flags.is_empty() {
                    if cfg!(windows) {
                        // On Windows, join flags and use STRING type
                        let flags_str = flags.join(" ");
                        options.push(format!("-DCMAKE_CXX_FLAGS:STRING={}", flags_str));
                        options.push(format!("-DCMAKE_C_FLAGS:STRING={}", flags_str));
                    } else {
                        // On Unix, use single quotes
                        let flags_str = flags.join(" ");
                        options.push(format!("-DCMAKE_CXX_FLAGS='{}'", flags_str));
                        options.push(format!("-DCMAKE_C_FLAGS='{}'", flags_str));
                    }
                }
            }
        }
    }

    options
}

pub fn apply_variant_settings(cmd: &mut Vec<String>, variant: &VariantSettings, config: &ProjectConfig) {
    let is_msvc_style = is_msvc_style_for_config(config);

    // The rest is the same
    if let Some(defines) = &variant.defines {
        for define in defines {
            cmd.push(format!("-D{}=1", define));
        }
    }

    if let Some(token_list) = &variant.flags {
        let real_flags = parse_universal_flags(token_list, is_msvc_style);
        if !real_flags.is_empty() {
            let joined = real_flags.join(" ");
            if is_msvc_style {
                cmd.push(format!("-DCMAKE_CXX_FLAGS:STRING={}", joined));
                cmd.push(format!("-DCMAKE_C_FLAGS:STRING={}", joined));
            } else {
                cmd.push(format!("-DCMAKE_CXX_FLAGS='{}'", joined));
                cmd.push(format!("-DCMAKE_C_FLAGS='{}'", joined));
            }
        }
    }

    // 4) apply any variant-level cmake_options
    if let Some(cmake_options) = &variant.cmake_options {
        cmd.extend(cmake_options.clone());
    }
}

pub fn get_active_variant<'a>(config: &'a ProjectConfig, requested_variant: Option<&str>) -> Option<&'a VariantSettings> {
    if let Some(variants) = &config.variants {
        // If a specific variant was requested, use that
        if let Some(requested) = requested_variant {
            return variants.variants.get(requested);
        }

        // Otherwise use the default variant from the config file
        if let Some(default_variant) = &variants.default {
            return variants.variants.get(default_variant);
        }
    }

    None
}

pub fn get_visual_studio_generator(requested_version: Option<&str>) -> String {
    println!("Detecting Visual Studio versions...");
    let versions = detect_visual_studio_versions();

    println!("Available Visual Studio versions: {:?}", versions
        .iter()
        .map(|(name, version)| format!("{} ({})", name, version))
        .collect::<Vec<_>>());

    println!("Requested version: {:?}", requested_version);

    // If a specific version is requested, try to find it
    if let Some(requested) = requested_version {
        for (name, _) in &versions {
            if name.to_lowercase().contains(&requested.to_lowercase()) {
                println!("Found matching VS version: {}", name);
                return name.clone();
            }
        }
        println!("No exact match for '{}', falling back to latest", requested);
    }

    // Otherwise return the latest (first in the list)
    if let Some((name, _)) = versions.first() {
        println!("Using latest VS version: {}", name);
        name.clone()
    } else {
        // Modern fallback
        println!("No VS versions detected, using fallback: Visual Studio 17 2022");


        "Visual Studio 17 2022".to_string()
    }
}

pub fn detect_visual_studio_versions() -> Vec<(String, String)> {
    let mut versions = Vec::new();

    // First, try vswhere (most reliable on modern Windows)
    let vswhere_success = if has_command("vswhere") {
        if let Ok(output) = Command::new("vswhere")
            .arg("-latest")
            .arg("-products")
            .arg("*")
            .arg("-requires")
            .arg("Microsoft.Component.MSBuild")
            .arg("-property")
            .arg("installationVersion")
            .output()
        {
            let version_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !version_str.is_empty() {
                // Parse major version
                if let Some(major) = version_str.split('.').next() {
                    let vs_name = match major {
                        "17" => "Visual Studio 17 2022",
                        "16" => "Visual Studio 16 2019",
                        "15" => "Visual Studio 15 2017",
                        "14" => "Visual Studio 14 2015",
                        "12" => "Visual Studio 12 2013",
                        _ => "Unknown Visual Studio",
                    };

                    versions.push((vs_name.to_string(), format!("{}.0", major)));
                    true
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };

    // If vswhere didn't work, try registry lookups
    if !vswhere_success {
        for (version, reg_key, generator) in &[
            ("17.0", "17.0", "Visual Studio 17 2022"),
            ("16.0", "16.0", "Visual Studio 16 2019"),
            ("15.0", "15.0", "Visual Studio 15 2017"),
            ("14.0", "14.0", "Visual Studio 14 2015"),
            ("12.0", "12.0", "Visual Studio 12 2013")
        ] {
            if let Ok(output) = Command::new("powershell")
                .arg("-Command")
                .arg(format!("(Get-ItemProperty -Path 'HKLM:\\SOFTWARE\\Microsoft\\VisualStudio\\SxS\\VS7' -Name '{}' -ErrorAction SilentlyContinue).'{}'", reg_key, reg_key))
                .output()
            {
                if !output.stdout.is_empty() {
                    versions.push((generator.to_string(), version.to_string()));
                }
            }
        }

        // Try to find Build Tools instead of full VS
        if versions.is_empty() {
            if let Ok(output) = Command::new("powershell")
                .arg("-Command")
                .arg("Get-ChildItem 'HKLM:\\SOFTWARE\\Microsoft\\VisualStudio\\SxS\\VS7'")
                .output()
            {
                let result = String::from_utf8_lossy(&output.stdout);

                // Very simple check - look for a common version number
                if result.contains("17.0") {
                    versions.push(("Visual Studio 17 2022".to_string(), "17.0".to_string()));
                } else if result.contains("16.0") {
                    versions.push(("Visual Studio 16 2019".to_string(), "16.0".to_string()));
                } else if result.contains("15.0") {
                    versions.push(("Visual Studio 15 2017".to_string(), "15.0".to_string()));
                }
            }
        }
    }

    // Try more direct detection: If we have cl.exe, try to determine its version
    if versions.is_empty() && has_command("cl") {
        if let Ok(output) = Command::new("cl").output() {
            let cl_version = String::from_utf8_lossy(&output.stderr);
            if cl_version.contains("19.30") || cl_version.contains("19.3") {
                versions.push(("Visual Studio 17 2022".to_string(), "17.0".to_string()));
            } else if cl_version.contains("19.20") || cl_version.contains("19.2") {
                versions.push(("Visual Studio 16 2019".to_string(), "16.0".to_string()));
            } else if cl_version.contains("19.1") {
                versions.push(("Visual Studio 15 2017".to_string(), "15.0".to_string()));
            }
        }
    }

    // If no version detected, provide modern fallbacks
    if versions.is_empty() {
        if has_command("cl") {
            // If cl.exe exists but we couldn't determine version, default to 2022
            versions.push(("Visual Studio 17 2022".to_string(), "17.0".to_string()));
        } else {
            // No VS detected but will still try to use a modern version
            versions.push(("Visual Studio 17 2022".to_string(), "17.0".to_string()));
        }
    }

    // Sort by version (newest first)
    versions.sort_by(|a, b| {
        let a_ver = a.1.split('.').next().unwrap_or("0").parse::<i32>().unwrap_or(0);
        let b_ver = b.1.split('.').next().unwrap_or("0").parse::<i32>().unwrap_or(0);
        b_ver.cmp(&a_ver)
    });

    versions
}

pub fn run_script(config: &ProjectConfig, script_name: &str, project_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(scripts) = &config.scripts {
        if let Some(script_command) = scripts.scripts.get(script_name) {
            println!("{}", format!("Running script: {}", script_name).blue());

            // Create command
            let shell = if cfg!(target_os = "windows") { "cmd" } else { "sh" };
            let shell_arg = if cfg!(target_os = "windows") { "/C" } else { "-c" };

            let mut command = Command::new(shell);
            command.arg(shell_arg).arg(script_command);
            command.current_dir(project_path);

            // Execute the command
            let output = command.output()?;

            // Print output
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            if !stdout.is_empty() {
                println!("{}", stdout);
            }

            if !output.status.success() {
                println!("{}", format!("Script '{}' failed:", script_name).red());
                if !stderr.is_empty() {
                    eprintln!("{}", stderr);
                }

                return Err(format!("Script failed with exit code: {}", output.status).into());
            }

            println!("{}", format!("Script '{}' completed successfully.", script_name).green());
            return Ok(());
        } else {
            return Err(format!("Script '{}' not found in configuration.", script_name).into());
        }
    }

    Err(format!("No scripts defined in configuration.").into())
}