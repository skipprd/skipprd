                        The ionCube Loader Wizard
                        -------------------------

This package contains:

* a Loader Wizard script to assist with Loader installation (loader-wizard.php)

* this README.txt file.


PURPOSE OF THE LOADER WIZARD
============================

The ionCube Loader Wizard assists in the installation of the appropriate 
ionCube Loader on a web server. The ionCube Loader is necessary to run 
PHP files that have been encoded with the ionCube Encoder, or to use
the real-time security and PHP error reporting features of the
ionCube24 service (https://ioncube24.com).


USING THE LOADER WIZARD
=======================

1. Upload the contents of this package to a directory/folder called ioncube
   within the top level of your web scripts area. This is sometimes called the
   "web root" or "document root". Common names for this location are "www",
   "public_html", and "htdocs", but it may be different on your server.

2. Launch the Loader Wizard script in your browser. For example:
         http://yourdomain/ioncube/loader-wizard.php

   If the Wizard is not found, check carefully the location on your server
   where you uploaded the Loaders and the wizard script.

3. Follow the steps given by the Loader Wizard. If you have full access to the
   server then installation should be easy. If your hosting plan is more
   limited, you may need to ask your hosting provider for assistance.

4. The Loader Wizard can automatically create a ticket in our support system
   if installation is unsuccessful, and we are happy to assist in that case.

   YouTube with a search for "ioncube loader wizard" also gives some helpful
   examples of installation.


WHERE TO INSTALL THE LOADERS
============================

The Loader Wizard should be used to guide the installation process but the
following are the standard locations for the Loader files. Loader file
packages can be obtained from https://www.ioncube.com/loaders.php 

Please check that you have the correct package of Loaders for your system.

Installing to a remote SHARED server
------------------------------------

* Upload the Loader files to a directory/folder called ioncube within your 
  main web scripts area.  (This will probably be where you placed the
  loader-wizard.php script.)


Installing to a remote UNIX/LINUX DEDICATED or VPS server
---------------------------------------------------------

* Upload the Loader files to the PHP extensions directory or, if that is 
  not set, /usr/local/ioncube


** Installing to a remote WINDOWS DEDICATED or VPS server

* Upload the Loader files to the PHP extensions directory or, if that is 
  not set, C:\windows\system32


64-BIT LOADERS FOR WINDOWS
--------------------------

64-bit Loaders for Windows are available for PHP 5.5 upwards. 
The Loader Wizard will not give directions for installing 64-bit Loaders for
any earlier version of PHP 5. 

Copyright (c) 2002-2020 ionCube Ltd.                  Last revised 26-Feb-2020
