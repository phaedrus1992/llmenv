use super::tests::{RULE, bash};

#[test]
fn the_option_writers_are_denied_for_a_temp_target_and_allowed_for_other_targets() {
    let denied = [
        "git clone https://host/repo.git /tmp/repo",
        "git clone --depth 1 -b main https://host/repo.git /tmp/repo",
        "git -C /tmp clone https://host/repo.git",
        "git clone --separate-git-dir /tmp/g https://host/repo.git repo",
        "curl -o /tmp/x https://host/x",
        "curl -sSo /tmp/x https://host/x",
        "curl -o/tmp/x https://host/x",
        "curl --output /tmp/x https://host/x",
        "curl --output=/tmp/x https://host/x",
        "curl --output-dir /tmp -O https://host/x",
        "wget -O /tmp/x https://host/x",
        "wget -O/tmp/x https://host/x",
        "wget -P /tmp https://host/x",
        "wget --output-document=/tmp/x https://host/x",
        "wget --directory-prefix /tmp https://host/x",
        "dd if=/dev/zero of=/tmp/x bs=1 count=1",
        "ln -s ./target /tmp/link",
        "ln -sf a b /tmp",
        "rsync -a src/ /tmp/dest/",
        "rsync -a --delete src /var/tmp/dest",
        "tar -xzf a.tgz -C /tmp",
        "tar xzf a.tgz -C /tmp",
        "tar -xf a.tar --directory=/tmp",
        "tar --extract --directory /tmp -f a.tar",
        "touch /var/tmp/x",
        "echo hi > /dev/shm/x",
    ];
    for command in denied {
        let reason = bash(command).unwrap_or_else(|| panic!("allowed: {command}"));
        assert!(reason.contains(RULE), "{command}: {reason}");
    }
    let allowed = [
        "git clone https://host/repo.git",
        "git clone https://host/repo.git ./repo",
        "git clone --depth 1 https://host/tmp/repo.git repo",
        "git -C /tmp status",
        "git -C /tmp log",
        "curl -o ./x https://host/tmp/x",
        "curl -O https://host/x",
        "curl https://host/x -H 'X: /tmp/y'",
        "curl -d @/tmp/body https://host/x",
        "wget -O ./x https://host/x",
        "wget -qO- https://host/x",
        "wget -P ./dir https://host/x",
        "dd if=/tmp/x of=./y",
        "ln -s /tmp/x ./link",
        "rsync -a /tmp/src ./dest",
        "rsync -a src host:/tmp/dest",
        "tar -cf out.tar -C /tmp x",
        "tar -czf out.tgz -C /tmp x",
        "tar -tf /tmp/a.tar",
        "tar -xzf /tmp/a.tgz -C ./dir",
        "tar xzf /tmp/a.tgz",
    ];
    for command in allowed {
        assert_eq!(bash(command), None, "denied: {command}");
    }
}

// The review of #4053 reproduced each bypass in this list.
#[test]
fn the_reviewed_bypasses_are_denied_and_their_near_misses_are_allowed() {
    let denied = [
        "rsync -t src /tmp/dest",
        "cp src -t/tmp/x",
        "cp -rt /tmp/x src",
        "install -t/tmp/d a",
        "mv --target-dir=/tmp a",
        "git clone --upload-pack x url /tmp/dst",
        "git clone --server-option x url /tmp/d",
        "git clone --bundle-uri b url /tmp/r",
        "git clone --ref-format files url /tmp/r",
        "git -C /tmp -C . clone url",
        "git -C /home/u -C ../../tmp clone url",
        "git --config-env a=b clone url /tmp/x",
        "git clone --sep=/tmp/g url r",
        "git --no-pager clone url /tmp/x",
        "tar -xTlist -C /tmp",
        "wget --output-doc=/tmp/x url",
        "wget --direc=/tmp url",
        "wget -o /tmp/log url",
        "wget -a /tmp/log url",
        "curl -D /tmp/h url",
        "curl -c /tmp/jar url",
        "curl --dump-header=/tmp/h url",
        "curl --cookie-j /tmp/jar url",
        "tar czf /tmp/b.tgz dir",
        "tar xfC a.tar /tmp",
        "tar xCf /tmp/dir a.tar",
        "tar -xf a --direc=/tmp",
        "tar cf /tmp/a.tar x",
        "tar --create --file=/tmp/a.tar x",
        "tar -rf /tmp/a.tar x",
    ];
    for command in denied {
        let reason = bash(command).unwrap_or_else(|| panic!("allowed: {command}"));
        assert!(reason.contains(RULE), "{command}: {reason}");
    }
    let allowed = [
        "rsync -t /tmp/s ./d",
        "rsync -t src ./dst",
        "curl -uroot:pw https://h/x",
        "curl -A ua https://h/x",
        "curl -Hfoo https://h/x",
        "curl -XPOST -d @/tmp/b https://h/x",
        "git clone --upload-pack x url ./dst",
        "git -C /tmp clone url /home/u/x",
        "git clone /tmp/src ./dst",
        "tar tf /tmp/a.tar",
        "tar xzf /tmp/a.tgz -C ./d",
        "tar cf out.tar -C /tmp x",
        "tar -cf out.tar /tmp/x",
        "tar xf /tmp/a.tar",
        "tar -xT -C /tmp",
        "tar xT -C /tmp",
        "tar xX -C /tmp",
        "git clone --reference /tmp/ref url ./dst",
        "git clone --se=/tmp/g url ./r",
        "git log clone url /tmp/x",
        "cp /tmp/a -- --",
    ];
    for command in allowed {
        assert_eq!(bash(command), None, "denied: {command}");
    }
}
