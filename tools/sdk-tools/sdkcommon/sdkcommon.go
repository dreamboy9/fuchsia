// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package sdkcommon

import (
	"bufio"
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io/ioutil"
	"os"
	"os/exec"
	"os/user"
	"path/filepath"
	"strings"

	"go.fuchsia.dev/fuchsia/tools/lib/color"
	"go.fuchsia.dev/fuchsia/tools/lib/logger"
)

var (
	// ExecCommand exports exec.Command as a variable so it can be mocked.
	ExecCommand = exec.Command
	// ExecLookPath exported to support mocking.
	ExecLookPath = exec.LookPath
	// logging support.
	logLevel = logger.InfoLevel
	log      = logger.NewLogger(logLevel, color.NewColor(color.ColorAuto), os.Stdout, os.Stderr, "sdk ")
)

// FuchsiaDevice represent a Fuchsia device.
type FuchsiaDevice struct {
	// IPv4 or IPv6 of the Fuchsia device.
	IpAddr string
	// Nodename of the Fuchsia device.
	Name string
}

// Default GCS bucket for prebuilt images and packages.
const defaultGCSbucket string = "fuchsia"

// GCSImage is used to return the bucket, name and version of a prebuilt.
type GCSImage struct {
	Bucket  string
	Name    string
	Version string
}

// Property keys used to get and set device configuration
const (
	DeviceNameKey  string = "device-name"
	BucketKey      string = "bucket"
	ImageKey       string = "image"
	DeviceIPKey    string = "device-ip"
	SSHPortKey     string = "ssh-port"
	PackageRepoKey string = "package-repo"
	PackagePortKey string = "package-port"
	DefaultKey     string = "default"
	// Top level key for storing data.
	deviceConfigurationKey string = "DeviceConfiguration"
	defaultDeviceKey       string = "_DEFAULT_DEVICE_"
)

const (
	defaultBucketName  string = "fuchsia"
	defaultSSHPort     string = "22"
	defaultPackagePort string = "8083"
)

var validPropertyNames = [...]string{
	DeviceNameKey,
	BucketKey,
	ImageKey,
	DeviceIPKey,
	SSHPortKey,
	PackageRepoKey,
	PackagePortKey,
	DefaultKey,
}

// DeviceConfig holds all the properties that are configured
// for a given devce.
type DeviceConfig struct {
	DeviceName  string `json:"device-name"`
	Bucket      string `json:"bucket"`
	Image       string `json:"image"`
	DeviceIP    string `json:"device-ip"`
	SSHPort     string `json:"ssh-port"`
	PackageRepo string `json:"package-repo"`
	PackagePort string `json:"package-port"`
	IsDefault   bool   `json:"default"`
}

// SDKProperties holds the common data for SDK tools.
// These values should be set or initialized by calling
// New().
type SDKProperties struct {
	dataPath                 string
	version                  string
	globalPropertiesFilename string
}

func (sdk SDKProperties) setDeviceDefaults(deviceConfig *DeviceConfig) DeviceConfig {
	// no reasonable default for device-name
	if deviceConfig.Bucket == "" {
		deviceConfig.Bucket = defaultBucketName
	}
	// no reasonable default for image
	// no reasonable default for device-ip
	if deviceConfig.SSHPort == "" {
		deviceConfig.SSHPort = defaultSSHPort
	}
	if deviceConfig.PackageRepo == "" {
		deviceConfig.PackageRepo = sdk.getDefaultPackageRepoDir(deviceConfig.DeviceName)
	}
	if deviceConfig.PackagePort == "" {
		deviceConfig.PackagePort = defaultPackagePort
	}
	return *deviceConfig
}

// Builds the data key for the given segments.
func getDeviceDataKey(segments []string) string {
	var fullKey = []string{deviceConfigurationKey}
	return strings.Join(append(fullKey, segments...), ".")
}

// DefaultGetUserHomeDir is the default implmentation of GetUserHomeDir()
// to allow mocking of user.Current()
func DefaultGetUserHomeDir() (string, error) {
	usr, err := user.Current()
	if err != nil {
		return "", nil
	}
	return usr.HomeDir, nil
}

// DefaultGetUsername is the default implmentation of GetUsername()
// to allow mocking of user.Current()
func DefaultGetUsername() (string, error) {
	usr, err := user.Current()
	if err != nil {
		return "", nil
	}
	return usr.Username, nil
}

// DefaultGetHostname is the default implmentation of GetHostname()
// to allow mocking of user.Current()
func DefaultGetHostname() (string, error) {
	return os.Hostname()
}

// GetUserHomeDir to allow mocking.
var GetUserHomeDir = DefaultGetUserHomeDir

// GetUsername to allow mocking.
var GetUsername = DefaultGetUsername

// GetHostname to allow mocking.
var GetHostname = DefaultGetHostname

func NewWithDataPath(dataPath string) (SDKProperties, error) {
	sdk := SDKProperties{}

	if dataPath != "" {
		sdk.dataPath = dataPath
	} else {
		homeDir, err := GetUserHomeDir()
		if err != nil {
			return sdk, err
		}
		sdk.dataPath = filepath.Join(homeDir, ".fuchsia")
	}

	toolsDir, err := sdk.GetToolsDir()
	if err != nil {
		return sdk, err
	}
	manifestFile, err := filepath.Abs(filepath.Join(toolsDir, "..", "..", "meta", "manifest.json"))
	if err != nil {
		return sdk, err
	}
	// If this is running in-tree, the manifest may not exist.
	if FileExists(manifestFile) {
		if sdk.version, err = readSDKVersion(manifestFile); err != nil {
			return sdk, err
		}
	}

	sdk.globalPropertiesFilename = filepath.Join(sdk.dataPath, "global_ffx_props.json")
	err = initFFXGlobalConfig(sdk)
	return sdk, err
}

// New creates an initialized SDKProperties using the default location
// for the data directory.
func New() (SDKProperties, error) {
	return NewWithDataPath("")
}

// GetSDKVersion returns the version of the SDK or empty if not set.
// Use sdkcommon.New() to create an initalized SDKProperties struct.
func (sdk SDKProperties) GetSDKVersion() string {
	return sdk.version
}

// GetSDKDataPath returns the path to the directory for storing SDK related data,
//  or empty if not set.
// Use sdkcommon.New() to create an initalized SDKProperties struct.
func (sdk SDKProperties) GetSDKDataPath() string {
	return sdk.dataPath
}

// getSDKVersion reads the manifest JSON file and returns the "id" property.
func readSDKVersion(manifestFilePath string) (string, error) {
	manifestFile, err := os.Open(manifestFilePath)
	// if we os.Open returns an error then handle it
	if err != nil {
		return "", err
	}
	defer manifestFile.Close()
	data, err := ioutil.ReadAll(manifestFile)
	if err != nil {
		return "", err
	}

	var result map[string]interface{}
	if err := json.Unmarshal([]byte(data), &result); err != nil {
		return "", err
	}

	version, _ := result["id"].(string)
	return version, nil
}

// GetDefaultPackageRepoDir returns the path to the package repository.
// If the value has been set with `fconfig`, use that value.
// Otherwise if there is a default target defined, return the target
// specific path.
// Lastly, if there is nothing, return the default repo path.
func (sdk SDKProperties) getDefaultPackageRepoDir(deviceName string) string {
	if deviceName != "" {
		return filepath.Join(sdk.GetSDKDataPath(), deviceName,
			"packages", "amber-files")
	}
	// As a last resort, `ffx` and the data are working as intended,
	// but no default has been configured, so fall back to the generic
	// legacy path.
	return filepath.Join(sdk.GetSDKDataPath(), "packages", "amber-files")
}

// GetDefaultDeviceName returns the name of the target device to use by default.
func (sdk SDKProperties) GetDefaultDeviceName() (string, error) {
	dataKey := getDeviceDataKey([]string{defaultDeviceKey})
	data, err := getDeviceConfigurationData(sdk, dataKey)
	if err != nil {
		return "", err
	}
	if name, ok := data[dataKey].(string); ok {
		return name, nil
	} else if len(data) == 0 {
		return "", nil
	}
	return "", fmt.Errorf("Cannot parse default device from %v", data)
}

// GetToolsDir returns the path to the SDK tools for the current
// CPU architecture. This is implemented by default of getting the
// directory of the currently exeecuting binary.
func (sdk SDKProperties) GetToolsDir() (string, error) {
	exePath, err := os.Executable()
	if err != nil {
		return "", fmt.Errorf("Could not currently running file: %v", err)
	}
	dir, err := filepath.Abs(filepath.Dir(exePath))
	if err != nil {
		return "", fmt.Errorf("could not get directory of currently running file: %s", err)
	}

	// This could be a symlink in a directory, so look for another common
	// tool (ffx). If it does not, try using the dir from argv[0].
	if FileExists(filepath.Join(dir, "ffx")) {
		return dir, nil
	}

	dir, err = filepath.Abs(filepath.Dir(os.Args[0]))
	if err != nil {
		return "", fmt.Errorf("Could not get path of argv[0]: %v", err)
	}
	return dir, nil
}

// GetAvailableImages returns the images available for the given version and bucket. If
// bucket is not the default bucket, the images in the default bucket are also returned.
func (sdk SDKProperties) GetAvailableImages(version string, bucket string) ([]GCSImage, error) {
	var buckets []string
	var images []GCSImage

	if bucket == "" || bucket == defaultGCSbucket {
		buckets = []string{defaultGCSbucket}
	} else {
		buckets = []string{bucket, defaultGCSbucket}
	}

	for _, b := range buckets {
		url := fmt.Sprintf("gs://%v/development/%v/images*", b, version)
		args := []string{"ls", url}
		output, err := runGSUtil(args)
		if err != nil {
			return images, err
		}
		for _, line := range strings.Split(strings.TrimSuffix(string(output), "\n"), "\n") {
			if len(filepath.Base(line)) >= 4 {
				bucketVersion := filepath.Base(filepath.Dir(filepath.Dir(line)))
				name := filepath.Base(line)[:len(filepath.Base(line))-4]
				images = append(images, GCSImage{Bucket: b, Version: bucketVersion, Name: name})
			} else {
				log.Warningf("Could not parse image name: %v", line)
			}
		}
	}
	return images, nil
}

// GetPackageSourcePath returns the GCS path for the given values.
func (sdk SDKProperties) GetPackageSourcePath(version string, bucket string, image string) string {
	return fmt.Sprintf("gs://%s/development/%s/packages/%s.tar.gz", bucket, version, image)
}

// RunFFXDoctor runs common checks for the ffx tool and host environment and returns
// the stdout.
func (sdk SDKProperties) RunFFXDoctor() (string, error) {
	args := []string{"doctor"}
	return sdk.RunFFX(args, false)
}

// GetAddressByName returns the IPv6 address of the device.
func (sdk SDKProperties) GetAddressByName(deviceName string) (string, error) {
	// Uses ffx disovery workflow by default. The legacy device-finder
	// workflow can be enabled by setting the environment variable FUCHSIA_DISABLED_ffx_discovery=1.
	FUCHSIA_DISABLED_FFX_DISCOVERY := os.Getenv("FUCHSIA_DISABLED_ffx_discovery")

	if FUCHSIA_DISABLED_FFX_DISCOVERY == "1" {
		toolsDir, err := sdk.GetToolsDir()
		if err != nil {
			return "", fmt.Errorf("Could not determine tools directory %v", err)
		}
		cmd := filepath.Join(toolsDir, "device-finder")
		args := []string{"resolve", "-device-limit", "1", "-ipv4=false", deviceName}
		output, err := ExecCommand(cmd, args...).Output()
		if err != nil {
			var exitError *exec.ExitError
			if errors.As(err, &exitError) {
				return "", fmt.Errorf("%v: %v", string(exitError.Stderr), exitError)
			} else {
				return "", err
			}
		}
		return strings.TrimSpace(string(output)), nil
	}
	// TODO(fxb/69008): use ffx json output.
	args := []string{"target", "list", "--format", "a", deviceName}
	output, err := sdk.RunFFX(args, false)
	if err != nil {
		var exitError *exec.ExitError
		if errors.As(err, &exitError) {
			return "", fmt.Errorf("%v: %v", string(exitError.Stderr), exitError)
		} else {
			return "", err
		}
	}
	return strings.TrimSpace(output), nil
}

func (f *FuchsiaDevice) String() string {
	return fmt.Sprintf("%s %s", f.IpAddr, f.Name)
}

// FindDeviceByName returns a fuchsia device matching a specific device name.
func (sdk SDKProperties) FindDeviceByName(deviceName string) (*FuchsiaDevice, error) {
	devices, err := sdk.ListDevices()
	if err != nil {
		return nil, err
	}
	for _, device := range devices {
		if device.Name == deviceName {
			return device, nil
		}
	}
	return nil, fmt.Errorf("no device with device name %s found", deviceName)
}

// FindDeviceByIP returns a fuchsia device matching a specific ip address.
func (sdk SDKProperties) FindDeviceByIP(ipAddr string) (*FuchsiaDevice, error) {
	devices, err := sdk.ListDevices()
	if err != nil {
		return nil, err
	}
	for _, device := range devices {
		if device.IpAddr == ipAddr {
			return device, nil
		}
	}
	return nil, fmt.Errorf("no device with IP address %s found", ipAddr)
}

// ListDevices returns all available fuchsia devices.
func (sdk SDKProperties) ListDevices() ([]*FuchsiaDevice, error) {
	var devices []*FuchsiaDevice
	var err error
	var output string
	// Uses ffx disovery workflow by default. The legacy device-finder
	// workflow can be enabled by setting the environment variable FUCHSIA_DISABLED_ffx_discovery=1.
	FUCHSIA_DISABLED_FFX_DISCOVERY := os.Getenv("FUCHSIA_DISABLED_ffx_discovery")
	if FUCHSIA_DISABLED_FFX_DISCOVERY == "1" {
		toolsDir, err := sdk.GetToolsDir()
		if err != nil {
			return nil, fmt.Errorf("Could not determine tools directory %v", err)
		}
		cmd := filepath.Join(toolsDir, "device-finder")

		args := []string{"list", "--full", "-ipv4=false"}

		outputAsBytes, err := ExecCommand(cmd, args...).Output()
		if err != nil {
			var exitError *exec.ExitError
			if errors.As(err, &exitError) {
				return nil, fmt.Errorf("%v: %v", string(exitError.Stderr), exitError)
			}
			return nil, err
		}
		output = string(outputAsBytes)
	} else {
		args := []string{"target", "list", "--format", "s"}
		output, err = sdk.RunFFX(args, false)
		if err != nil {
			return nil, err
		}
	}

	for _, line := range strings.Split(output, "\n") {
		parts := strings.Split(line, " ")
		if len(parts) == 2 {
			devices = append(devices, &FuchsiaDevice{
				IpAddr: strings.TrimSpace(parts[0]),
				Name:   strings.TrimSpace(parts[1]),
			})
		}
	}

	if len(devices) < 1 {
		return nil, fmt.Errorf("no devices found")
	}
	return devices, nil
}

func getCommonSSHArgs(sdk SDKProperties, customSSHConfig string, privateKey string,
	sshPort string) []string {

	var cmdArgs []string
	if customSSHConfig != "" {
		cmdArgs = append(cmdArgs, "-F", customSSHConfig)
	} else {
		cmdArgs = append(cmdArgs, "-F", getFuchsiaSSHConfigFile(sdk))
	}
	if privateKey != "" {
		cmdArgs = append(cmdArgs, "-i", privateKey)
	}
	if sshPort != "" {
		cmdArgs = append(cmdArgs, "-p", sshPort)
	}

	return cmdArgs
}

// RunSFTPCommand runs sftp (one of SSH's file copy tools).
// Setting to_target to true will copy file SRC from host to DST on the target.
// Otherwise it will copy file from SRC from target to DST on the host.
// sshPort if non-empty will use this port to connect to the device.
// The return value is the error if any.
func (sdk SDKProperties) RunSFTPCommand(targetAddress string, customSSHConfig string, privateKey string,
	sshPort string, to_target bool, src string, dst string) error {

	commonArgs := []string{"-q", "-b", "-"}
	if customSSHConfig == "" || privateKey == "" {
		if err := checkSSHConfig(sdk); err != nil {
			return err
		}
	}

	cmdArgs := getCommonSSHArgs(sdk, customSSHConfig, privateKey, sshPort)

	cmdArgs = append(cmdArgs, commonArgs...)
	if targetAddress == "" {
		return errors.New("target address must be specified")
	}
	// SFTP needs the [] around the ipv6 address, which is different than ssh.
	if strings.Contains(targetAddress, ":") {
		targetAddress = fmt.Sprintf("[%v]", targetAddress)
	}
	cmdArgs = append(cmdArgs, targetAddress)

	stdin := ""

	if to_target {
		stdin = fmt.Sprintf("put %v %v", src, dst)
	} else {
		stdin = fmt.Sprintf("get %v %v", src, dst)
	}

	return runSFTP(cmdArgs, stdin)
}

// RunSSHCommand runs the command provided in args on the given target device.
// The customSSHconfig is optional and overrides the SSH configuration defined by the SDK.
// privateKey is optional to specify a private key to use to access the device.
// verbose adds the -v flag to ssh.
// sshPort if non-empty is used as the custom ssh port on the commandline.
// The return value is the stdout.
func (sdk SDKProperties) RunSSHCommand(targetAddress string, customSSHConfig string,
	privateKey string, sshPort string, verbose bool, args []string) (string, error) {

	cmdArgs, err := buildSSHArgs(sdk, targetAddress, customSSHConfig, privateKey, sshPort, verbose, args)
	if err != nil {
		return "", err
	}

	return runSSH(cmdArgs, false)
}

// RunSSHShell runs the command provided in args on the given target device and
// uses the system stdin, stdout, stderr.
// Returns when the ssh process exits.
// The customSSHconfig is optional and overrides the SSH configuration defined by the SDK.
// privateKey is optional to specify a private key to use to access the device.
// sshPort if non-empty is used as the custom ssh port on the commandline.
// verbose adds the -v flag to ssh.
// The return value is the stdout.
func (sdk SDKProperties) RunSSHShell(targetAddress string, customSSHConfig string,
	privateKey string, sshPort string, verbose bool, args []string) error {

	cmdArgs, err := buildSSHArgs(sdk, targetAddress, customSSHConfig, privateKey,
		sshPort, verbose, args)
	if err != nil {
		return err
	}
	_, err = runSSH(cmdArgs, true)
	return err

}

func buildSSHArgs(sdk SDKProperties, targetAddress string, customSSHConfig string,
	privateKey string, sshPort string, verbose bool, args []string) ([]string, error) {
	if customSSHConfig == "" || privateKey == "" {
		if err := checkSSHConfig(sdk); err != nil {
			return []string{}, err
		}
	}

	cmdArgs := getCommonSSHArgs(sdk, customSSHConfig, privateKey, sshPort)
	if verbose {
		cmdArgs = append(cmdArgs, "-v")
	}

	if targetAddress == "" {
		return cmdArgs, errors.New("target address must be specified")
	}
	cmdArgs = append(cmdArgs, targetAddress)

	cmdArgs = append(cmdArgs, args...)

	return cmdArgs, nil
}

func getFuchsiaSSHConfigFile(sdk SDKProperties) string {
	return filepath.Join(sdk.GetSDKDataPath(), "sshconfig")
}

/* This function creates the ssh keys needed to
 work with devices running Fuchsia. There are two parts, the keys and the config.

 There is a key for Fuchsia that is placed in a well-known location so that applications
 which need to access the Fuchsia device can all use the same key. This is stored in
 ${HOME}/.ssh/fuchsia_ed25519.

 The authorized key file used for paving is in ${HOME}/.ssh/fuchsia_authorized_keys.
 The private key used when ssh'ing to the device is in ${HOME}/.ssh/fuchsia_ed25519.


 The second part of is the sshconfig file used by the SDK when using SSH.
 This is stored in the Fuchsia SDK data directory named sshconfig.
 This script checks for the private key file being referenced in the sshconfig and
the matching version tag. If they are not present, the sshconfig file is regenerated.
*/

const sshConfigTag = "Fuchsia SDK config version 5 tag"

func checkSSHConfig(sdk SDKProperties) error {
	// The ssh configuration should not be modified.

	homeDir, err := GetUserHomeDir()
	if err != nil {
		return fmt.Errorf("SSH configuration requires a $HOME directory: %v", err)
	}
	userName, err := GetUsername()
	if err != nil {
		return fmt.Errorf("SSH configuration requires a user name: %v", err)
	}
	var (
		sshDir        = filepath.Join(homeDir, ".ssh")
		authFile      = filepath.Join(sshDir, "fuchsia_authorized_keys")
		keyFile       = filepath.Join(sshDir, "fuchsia_ed25519")
		sshConfigFile = getFuchsiaSSHConfigFile(sdk)
	)
	// If the public and private key pair exist, and the sshconfig
	// file is up to date, then our work here is done, return success.
	if FileExists(authFile) && FileExists(keyFile) && FileExists(sshConfigFile) {
		config, err := ioutil.ReadFile(sshConfigFile)
		if err == nil {
			if strings.Contains(string(config), sshConfigTag) {
				return nil
			}
		}
		// The version tag does not match, so remove the old config file.
		os.Remove(sshConfigFile)
	}

	if err := os.MkdirAll(sshDir, 0755); err != nil {
		return fmt.Errorf("Could not create %v: %v", sshDir, err)
	}

	// Check to migrate keys from old location
	if !FileExists(authFile) || !FileExists(keyFile) {
		if err := moveLegacyKeys(sdk, authFile, keyFile); err != nil {
			return fmt.Errorf("Could not migrate legacy SSH keys: %v", err)
		}
	}

	// Create keys if needed
	if !FileExists(authFile) || !FileExists(keyFile) {
		if !FileExists(keyFile) {
			hostname, _ := GetHostname()
			if hostname == "" {
				hostname = "unknown"
			}
			if err := generateSSHKey(keyFile, userName, hostname); err != nil {
				return fmt.Errorf("Could generate private SSH key: %v", err)
			}
		}
		if err := generatePublicSSHKeyfile(keyFile, authFile); err != nil {
			return fmt.Errorf("Could get public keys from private SSH key: %v", err)
		}
	}

	if err := writeSSHConfigFile(sshConfigFile, sshConfigTag, keyFile); err != nil {
		return fmt.Errorf("Could write sshconfig file %v: %v", sshConfigFile, err)
	}
	return nil
}

func generateSSHKey(keyFile string, username string, hostname string) error {
	path, err := ExecLookPath("ssh-keygen")
	if err != nil {
		return fmt.Errorf("could not find ssh-keygen on path: %v", err)
	}
	args := []string{
		"-P", "",
		"-t", "ed25519",
		"-f", keyFile,
		"-C", fmt.Sprintf("%v@%v generated by Fuchsia GN SDK", username, hostname),
	}
	cmd := ExecCommand(path, args...)
	_, err = cmd.Output()
	if err != nil {
		var exitError *exec.ExitError
		if errors.As(err, &exitError) {
			return fmt.Errorf("%v: %v", string(exitError.Stderr), exitError)
		} else {
			return err
		}
	}
	return nil
}

func generatePublicSSHKeyfile(keyFile string, authFile string) error {
	path, err := ExecLookPath("ssh-keygen")
	if err != nil {
		return fmt.Errorf("could not find ssh-keygen on path: %v", err)
	}
	args := []string{
		"-y",
		"-f", keyFile,
	}
	cmd := ExecCommand(path, args...)
	publicKey, err := cmd.Output()
	if err != nil {
		var exitError *exec.ExitError
		if errors.As(err, &exitError) {
			return fmt.Errorf("%v: %v", string(exitError.Stderr), exitError)
		} else {
			return err
		}
	}

	if err := os.MkdirAll(filepath.Dir(authFile), 0755); err != nil {
		return err
	}

	output, err := os.Create(authFile)
	if err != nil {
		return err
	}
	defer output.Close()

	fmt.Fprintln(output, publicKey)
	return nil
}

func writeSSHConfigFile(sshConfigFile string, versionTag string, keyFile string) error {

	if err := os.MkdirAll(filepath.Dir(sshConfigFile), 0755); err != nil {
		return err
	}

	output, err := os.Create(sshConfigFile)
	if err != nil {
		return err
	}
	defer output.Close()

	fmt.Fprintf(output, "# %s\n", versionTag)
	fmt.Fprintf(output,
		`# Configure port 8022 for connecting to a device with the local address.
# This makes it possible to forward 8022 to a device connected remotely.
# The fuchsia private key is used for the identity.
Host 127.0.0.1
	Port 8022

Host ::1
	Port 8022

Host *
# Turn off refusing to connect to hosts whose key has changed
StrictHostKeyChecking no
CheckHostIP no

# Disable recording the known hosts
UserKnownHostsFile=/dev/null

# Do not forward auth agent connection to remote, no X11
ForwardAgent no
ForwardX11 no

# Connection timeout in seconds
ConnectTimeout=10

# Check for server alive in seconds, max count before disconnecting
ServerAliveInterval 1
ServerAliveCountMax 10

# Try to keep the master connection open to speed reconnecting.
ControlMaster auto
ControlPersist yes

# When expanded, the ControlPath below cannot have more than 90 characters
# (total of 108 minus 18 used by a random suffix added by ssh).
# '%%C' expands to 40 chars and there are 9 fixed chars, so '~' can expand to
# up to 41 chars, which is a reasonable limit for a user's home in most
# situations. If '~' expands to more than 41 chars, the ssh connection
# will fail with an error like:
#     unix_listener: path "..." too long for Unix domain socket
# A possible solution is to use /tmp instead of ~, but it has
# its own security concerns.
ControlPath=~/.ssh/fx-%%C

# Connect with user, use the identity specified.
User fuchsia
IdentitiesOnly yes
IdentityFile "%v"
GSSAPIDelegateCredentials no

`, keyFile)

	return nil
}

func moveLegacyKeys(sdk SDKProperties, destAuthFile string, destKeyFile string) error {

	// Check for legacy GN SDK key and copy it to the new location.
	var (
		legacySSHDir   = filepath.Join(sdk.GetSDKDataPath(), ".ssh")
		legacyKeyFile  = filepath.Join(legacySSHDir, "pkey")
		legacyAuthFile = filepath.Join(legacySSHDir, "authorized_keys")
	)
	if FileExists(legacyKeyFile) {
		fmt.Fprintf(os.Stderr, "Migrating legacy key file %v to %v\n", legacyKeyFile, destKeyFile)
		if err := os.Rename(legacyKeyFile, destKeyFile); err != nil {
			return err
		}
		if FileExists(legacyAuthFile) {
			if err := os.Rename(legacyAuthFile, destAuthFile); err != nil {
				return err
			}
		}
	}
	return nil
}

// GetValidPropertyNames returns the list of valid properties for a
// device configuration.
func (sdk SDKProperties) GetValidPropertyNames() []string {
	return validPropertyNames[:]
}

// IsValidProperty returns true if the property is a valid
// property name.
func (sdk SDKProperties) IsValidProperty(property string) bool {
	for _, item := range validPropertyNames {
		if item == property {
			return true
		}
	}
	return false
}

// GetFuchsiaProperty returns the value for the given property for the given device.
// If the device name is empty, the default device is used via GetDefaultDeviceName().
// It is an error if the property cannot be found.
func (sdk SDKProperties) GetFuchsiaProperty(device string, property string) (string, error) {
	var err error
	deviceName := device
	if deviceName == "" {
		if deviceName, err = sdk.GetDefaultDeviceName(); err != nil {
			return "", err
		}
	}
	deviceConfig, err := sdk.GetDeviceConfiguration(deviceName)
	if err != nil {
		return "", fmt.Errorf("Could not read configuration data for %v : %v", deviceName, err)
	}

	switch property {
	case BucketKey:
		return deviceConfig.Bucket, nil
	case DeviceIPKey:
		return deviceConfig.DeviceIP, nil
	case DeviceNameKey:
		return deviceConfig.DeviceName, nil
	case ImageKey:
		return deviceConfig.Image, nil
	case PackagePortKey:
		return deviceConfig.PackagePort, nil
	case PackageRepoKey:
		return deviceConfig.PackageRepo, nil
	case SSHPortKey:
		return deviceConfig.SSHPort, nil
	}
	return "", fmt.Errorf("Could not find property %v.%v", deviceName, property)
}

// GetDeviceConfigurations returns a list of all device configurations.
func (sdk SDKProperties) GetDeviceConfigurations() ([]DeviceConfig, error) {
	var configs []DeviceConfig

	// Get all config data.
	configData, err := getDeviceConfigurationData(sdk, deviceConfigurationKey)
	if err != nil {
		return configs, fmt.Errorf("Could not read configuration data : %v", err)
	}
	if len(configData) == 0 {
		return configs, nil
	}

	defaultDeviceName, err := sdk.GetDefaultDeviceName()
	if err != nil {
		return configs, err
	}

	if deviceConfigMap, ok := configData[deviceConfigurationKey].(map[string]interface{}); ok {
		for k, v := range deviceConfigMap {
			if !isReservedProperty(k) {
				if device, ok := mapToDeviceConfig(v); ok {
					device.IsDefault = defaultDeviceName == device.DeviceName
					configs = append(configs, device)
				}
			}
		}
		return configs, nil

	}
	return configs, fmt.Errorf("Could not read configuration data: %v", configData)
}

// GetDeviceConfiguration returns the configuration for the device with the given name.
func (sdk SDKProperties) GetDeviceConfiguration(name string) (DeviceConfig, error) {
	var deviceConfig DeviceConfig

	dataKey := getDeviceDataKey([]string{name})
	configData, err := getDeviceConfigurationData(sdk, dataKey)
	if err != nil {
		return deviceConfig, fmt.Errorf("Could not read configuration data : %v", err)
	}
	if len(configData) == 0 {
		sdk.setDeviceDefaults(&deviceConfig)
		return deviceConfig, nil
	}

	if deviceData, ok := configData[dataKey]; ok {
		if deviceConfig, ok := mapToDeviceConfig(deviceData); ok {
			defaultDeviceName, err := sdk.GetDefaultDeviceName()
			if err != nil {
				return deviceConfig, err
			}
			deviceConfig.IsDefault = deviceConfig.DeviceName == defaultDeviceName
			// Set the default values for the  device, even if not set explicitly
			// This centralizes the configuration into 1 place.
			sdk.setDeviceDefaults(&deviceConfig)
			return deviceConfig, nil
		}
		return deviceConfig, fmt.Errorf("Cannot parse DeviceConfig from %v", configData)
	}
	return deviceConfig, fmt.Errorf("Cannot parse DeviceData.%v from %v", name, configData)
}

// SaveDeviceConfiguration persists the given device configuration properties.
func (sdk SDKProperties) SaveDeviceConfiguration(newConfig DeviceConfig) error {

	// Create a map of key to value to store. Only write out values that are explicitly set to something
	// that is not the default.
	origConfig, err := sdk.GetDeviceConfiguration(newConfig.DeviceName)
	if err != nil {
		return err
	}
	var defaultConfig = DeviceConfig{}
	sdk.setDeviceDefaults(&defaultConfig)

	dataMap := make(map[string]string)
	dataMap[getDeviceDataKey([]string{newConfig.DeviceName, DeviceNameKey})] = newConfig.DeviceName
	// if the value changed from the orginal, write it out.
	if origConfig.Bucket != newConfig.Bucket {
		dataMap[getDeviceDataKey([]string{newConfig.DeviceName, BucketKey})] = newConfig.Bucket
	} else if defaultConfig.Bucket == newConfig.Bucket {
		// if the new value is the default value, then write the empty string.
		dataMap[getDeviceDataKey([]string{newConfig.DeviceName, BucketKey})] = ""
	}
	if origConfig.DeviceIP != newConfig.DeviceIP {
		dataMap[getDeviceDataKey([]string{newConfig.DeviceName, DeviceIPKey})] = newConfig.DeviceIP
	} else if defaultConfig.DeviceIP == newConfig.DeviceIP {
		dataMap[getDeviceDataKey([]string{newConfig.DeviceName, DeviceIPKey})] = ""
	}
	if origConfig.Image != newConfig.Image {
		dataMap[getDeviceDataKey([]string{newConfig.DeviceName, ImageKey})] = newConfig.Image
	} else if defaultConfig.Image == newConfig.Image {
		dataMap[getDeviceDataKey([]string{newConfig.DeviceName, ImageKey})] = ""
	}
	if origConfig.PackagePort != newConfig.PackagePort {
		dataMap[getDeviceDataKey([]string{newConfig.DeviceName, PackagePortKey})] = newConfig.PackagePort
	} else if defaultConfig.PackagePort == newConfig.PackagePort {
		dataMap[getDeviceDataKey([]string{newConfig.DeviceName, PackagePortKey})] = ""
	}
	if origConfig.PackageRepo != newConfig.PackageRepo {
		dataMap[getDeviceDataKey([]string{newConfig.DeviceName, PackageRepoKey})] = newConfig.PackageRepo
	} else if defaultConfig.PackageRepo == newConfig.PackageRepo {
		dataMap[getDeviceDataKey([]string{newConfig.DeviceName, PackageRepoKey})] = ""
	}
	if origConfig.SSHPort != newConfig.SSHPort {
		dataMap[getDeviceDataKey([]string{newConfig.DeviceName, SSHPortKey})] = newConfig.SSHPort
	} else if defaultConfig.SSHPort == newConfig.SSHPort {
		dataMap[getDeviceDataKey([]string{newConfig.DeviceName, SSHPortKey})] = ""
	}
	if newConfig.IsDefault {
		dataMap[getDeviceDataKey([]string{defaultDeviceKey})] = newConfig.DeviceName
	}

	for key, value := range dataMap {
		err := writeConfigurationData(sdk, key, value)
		if err != nil {
			return err
		}
	}
	return nil
}

// RemoveDeviceConfiguration removes the device settings for the given name.
func (sdk SDKProperties) RemoveDeviceConfiguration(deviceName string) error {
	dataKey := getDeviceDataKey([]string{deviceName})

	args := []string{"config", "remove", "--level", "global", dataKey}

	if _, err := sdk.RunFFX(args, false); err != nil {
		return fmt.Errorf("Error removing %s configuration: %v", deviceName, err)
	}

	defaultDeviceName, err := sdk.GetDefaultDeviceName()
	if err != nil {
		return err
	}
	if defaultDeviceName == deviceName {
		err := writeConfigurationData(sdk, getDeviceDataKey([]string{defaultDeviceKey}), "")
		if err != nil {
			return err
		}
	}
	return nil
}

// ResolveTargetAddress evaulates the deviceIP and deviceName  passed in
// to determine the target IP address. This include consulting the configuration
// information set via `fconfig`.
func (sdk SDKProperties) ResolveTargetAddress(deviceIP string, deviceName string) (string, error) {
	var (
		targetAddress string
		err           error
	)

	helpfulTipMsg := `Try running "ffx target list --format s" and then "fconfig set-device <device_name> --image <image_name> --default".`

	// If  there is a deviceIP address, use it.
	if deviceIP != "" {
		targetAddress = deviceIP
	} else {
		// No explicit address, use the name
		if deviceName == "" {
			// No name passed in, use the default name.
			if deviceName, err = sdk.GetDefaultDeviceName(); err != nil {
				return "", fmt.Errorf("could not determine default device name.\n%v %v", helpfulTipMsg, err)
			}
		}
		if deviceName == "" {
			// No address specified, no device name specified, and no device configured as the default.
			return "", fmt.Errorf("invalid arguments. Need to specify --device-ip or --device-name or use fconfig to configure a default device.\n%v", helpfulTipMsg)
		}

		// look up a configured address by devicename
		targetAddress, err = sdk.GetFuchsiaProperty(deviceName, DeviceIPKey)
		if err != nil {
			return "", fmt.Errorf("could not read configuration information for %v.\n%v %v", deviceName, helpfulTipMsg, err)
		}
		// if still nothing, resolve the device address by name
		if targetAddress == "" {
			if targetAddress, err = sdk.GetAddressByName(deviceName); err != nil {
				return "", fmt.Errorf(`cannot get target address for %v.
Try running "ffx target list --format s" and verify the name matches in "fconfig get-all". %v`, deviceName, err)
			}
		}
	}
	if targetAddress == "" {
		return "", fmt.Errorf(`could not get target device IP address for %v.
Try running "ffx target list --format s" and verify the name matches in "fconfig get-all".`, deviceName)
	}
	return targetAddress, nil
}

func initFFXGlobalConfig(sdk SDKProperties) error {
	args := []string{"config", "env"}
	var (
		err    error
		output string
		line   string
	)
	if output, err = sdk.RunFFX(args, false); err != nil {
		return fmt.Errorf("Error getting config environment %v", err)
	}
	reader := bufio.NewReader(bytes.NewReader([]byte(output)))
	hasGlobal := false
	for !hasGlobal {
		line, err = reader.ReadString('\n')
		if err != nil {
			if err.Error() == "EOF" {
				break
			} else {
				return err
			}
		}
		if strings.HasPrefix(strings.TrimSpace(line), "Global") {
			break
		}
	}
	doSetEnv := len(line) == 0
	if len(line) > 0 {
		const (
			prefix    = "Global:"
			prefixLen = len(prefix)
		)
		index := strings.Index(line, "Global:")
		if index > len(line) {
			return fmt.Errorf("Cannot parse `Global:` prefix from %v", line)
		}
		filename := strings.TrimSpace(line[index+prefixLen:])
		_, err := os.Stat(filename)
		doSetEnv = os.IsNotExist(err)

	}
	if doSetEnv {
		// Create the global config level
		if len(sdk.globalPropertiesFilename) == 0 {
			return fmt.Errorf("Cannot initialize property config, global file name is empty: %v", sdk)
		}
		args := []string{"config", "env", "set", sdk.globalPropertiesFilename, "--level", "global"}

		if _, err := sdk.RunFFX(args, false); err != nil {
			var exitError *exec.ExitError
			if errors.As(err, &exitError) {
				return fmt.Errorf("Error initializing global properties environment: %v %v: %v", args, string(exitError.Stderr), exitError)
			} else {
				return fmt.Errorf("Error initializing global properties environment: %v %v", args, err)
			}
		}
	}
	return nil
}

// writeConfigurationData calls `ffx` to store the value at the specified key.
func writeConfigurationData(sdk SDKProperties, key string, value string) error {
	args := []string{"config", "set", "--level", "global", key, value}
	if output, err := sdk.RunFFX(args, false); err != nil {
		return fmt.Errorf("Error writing %v = %v: %v %v", key, value, err, output)
	}
	return nil
}

// getDeviceConfigurationData calls `ffx` to read the data at the specified key.
func getDeviceConfigurationData(sdk SDKProperties, key string) (map[string]interface{}, error) {
	var (
		data   map[string]interface{}
		err    error
		output string
	)

	args := []string{"config", "get", key}

	if output, err = sdk.RunFFX(args, false); err != nil {
		// Exit code of 2 means no value was found.
		if exiterr, ok := err.(*exec.ExitError); ok && exiterr.ExitCode() == 2 {
			return data, nil
		}
		return data, fmt.Errorf("Error reading %v: %v %v", key, err, output)
	}

	if len(output) > 0 {
		jsonString := string(output)

		// wrap the response in {} and double quote the key so it is suitable for json unmarshaling.
		fullJSONString := "{\"" + key + "\": " + jsonString + "}"
		err := json.Unmarshal([]byte(fullJSONString), &data)
		if err != nil {
			return data, fmt.Errorf("Error parsing configuration data %v: %s", err, fullJSONString)
		}
	}
	return data, nil
}

// RunFFX executes ffx with the given args, returning stdout. If there is an error,
// the error will usually be of type *ExitError.
func (sdk SDKProperties) RunFFX(args []string, interactive bool) (string, error) {
	toolsDir, err := sdk.GetToolsDir()
	if err != nil {
		return "", fmt.Errorf("Could not determine tools directory %v", err)
	}
	cmd := filepath.Join(toolsDir, "ffx")

	ffx := ExecCommand(cmd, args...)
	if interactive {
		ffx.Stderr = os.Stderr
		ffx.Stdout = os.Stdout
		ffx.Stdin = os.Stdin
		return "", ffx.Run()
	}
	output, err := ffx.Output()
	if err != nil {
		return "", err
	}
	return string(output), err
}

// isReservedProperty used to differenciate between properties used
// internally and device names.
func isReservedProperty(property string) bool {
	switch property {
	case defaultDeviceKey:
		return true
	}
	return false
}

// mapToDeviceConfig converts the map returned by json into a DeviceConfig struct.
func mapToDeviceConfig(data interface{}) (DeviceConfig, bool) {
	var (
		device     DeviceConfig
		deviceData map[string]interface{}
		ok         bool
		value      string
	)

	if deviceData, ok = data.(map[string]interface{}); ok {
		for _, key := range validPropertyNames {
			// the Default flag is stored else where, so don't try to
			// key it from the map.
			if key == DefaultKey {
				continue
			}
			// Use Sprintf to convert the value into a string.
			// This is done since some values are numeric and are
			// not unmarshalled as strings.
			if val, ok := deviceData[key]; ok {
				value = fmt.Sprintf("%v", val)
			} else {
				fmt.Fprintf(os.Stderr, "Cannot get %v from %v", key, deviceData)
				continue
			}
			switch key {
			case BucketKey:
				device.Bucket = value
			case DeviceIPKey:
				device.DeviceIP = value
			case DeviceNameKey:
				device.DeviceName = value
			case ImageKey:
				device.Image = value
			case PackagePortKey:
				device.PackagePort = value
			case PackageRepoKey:
				device.PackageRepo = value
			case SSHPortKey:
				device.SSHPort = value
			}
		}
	}
	return device, ok
}
