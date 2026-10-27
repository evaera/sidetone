'use strict';
const { app } = require('electron');
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const root = process.env.SIDETONE_NOISE_CANCELLATION_ROOT;
app.setPath('userData', path.join(root, 'profile'));
app.setPath('sessionData', path.join(root, 'profile'));
app.disableHardwareAcceleration();
app.whenReady().then(() => {
  try {
    const modulePath = path.join(root, 'module/discord_krisp.node');
    const hash = crypto.createHash('sha256').update(fs.readFileSync(modulePath)).digest('hex');
    if (hash !== '2cf8a2c6cc818243b6bb32ca542d6777ba20cd3dd9137f426bcd3b392e156e9b') {
      throw new Error('Installed Noise cancellation module differs from the tested ABI. Revalidation required.');
    }
    process.env.SIDETONE_NOISE_CANCELLATION_MODULE = modulePath;
    const noise_cancellation = require(modulePath);
    noise_cancellation._initialize({ logDirectory: root, logLevel: 0 });
    require(path.join(root, 'sidetone-noise-cancellation.node'));
    app.exit(0);
  } catch (error) {
    fs.writeFileSync(path.join(root, 'helper-error.txt'), String(error.stack || error));
    app.exit(1);
  }
});
